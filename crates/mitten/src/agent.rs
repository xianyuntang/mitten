//! Agent loop: OpenCode Go's Anthropic-format Messages API + a client-side bash tool.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::config::Config;
use crate::db::Db;

// ponytail: hard cap on tool output fed back to the model; summarize or page if it bites.
const MAX_OUTPUT_CHARS: usize = 30_000;

/// Where the agent talks to the user: the terminal or a chat channel.
pub trait Io {
    /// Shows model text to the user.
    async fn say(&mut self, text: &str) -> Result<()>;
    /// Asks the user to approve `command`; anything but an explicit yes is a no.
    async fn confirm(&mut self, command: &str) -> Result<bool>;
}

/// One conversation, kept in memory for the process lifetime.
#[derive(Debug)]
pub struct Agent {
    http: reqwest::Client,
    config: Config,
    db: Db,
    conversation_id: i64,
    system: String,
    messages: Vec<Value>,
}

impl Agent {
    /// Resumes the stored conversation named `key` (e.g. `terminal`, `discord:<channel>`).
    pub async fn new(config: Config, db: Db, key: &str) -> Result<Self> {
        let conversation = db.conversation(key).await?;
        let messages = db
            .messages(conversation.id)
            .await?
            .iter()
            .map(crate::db::Message::to_api)
            .collect();
        let cwd = std::env::current_dir().context("failed to read current directory")?;
        let system = format!(
            "You are Mitten, a personal agent running on the user's machine ({os}). \
             Use the bash tool to inspect and change things; the user approves each command. \
             Working directory: {cwd}.",
            os = std::env::consts::OS,
            cwd = cwd.display(),
        );
        Ok(Self {
            http: http_client()?,
            config,
            db,
            conversation_id: conversation.id,
            system,
            messages,
        })
    }

    pub fn describe(&self) -> String {
        format!("opencode-go / {}", self.config.model)
    }

    /// Runs one user turn to completion, looping while the model calls tools, then saves it.
    /// On failure the turn is rolled back so the history stays valid.
    pub async fn run_turn(&mut self, prompt: &str, io: &mut impl Io) -> Result<()> {
        let turn_start = self.messages.len();
        if let Err(err) = self.drive(prompt, io).await {
            self.messages.truncate(turn_start);
            return Err(err);
        }
        self.db
            .append(self.conversation_id, self.messages[turn_start..].to_vec())
            .await
            .context("failed to save the conversation")
    }

    /// Forgets the conversation, in memory and on disk.
    pub async fn reset(&mut self) -> Result<()> {
        self.db.clear(self.conversation_id).await?;
        self.messages.clear();
        Ok(())
    }

    async fn drive(&mut self, prompt: &str, io: &mut impl Io) -> Result<()> {
        self.messages
            .push(json!({"role": "user", "content": prompt}));
        loop {
            let response = self.call().await?;
            let stop_reason = response["stop_reason"].as_str().unwrap_or_default();
            match stop_reason {
                "refusal" => bail!("the model declined this request (stop_reason: refusal)"),
                "max_tokens" => bail!("response hit max_tokens ({})", self.config.max_tokens),
                _ => {}
            }

            let content = response["content"].clone();
            for block in content.as_array().into_iter().flatten() {
                if let Some(text) = block["text"].as_str().filter(|_| block["type"] == "text") {
                    io.say(text).await?;
                }
            }
            // Append the full content (thinking blocks included) unchanged.
            self.messages
                .push(json!({"role": "assistant", "content": content}));

            if stop_reason != "tool_use" {
                return Ok(());
            }

            let mut results = Vec::new();
            for block in content.as_array().into_iter().flatten() {
                if block["type"] == "tool_use" {
                    results.push(run_tool(block, io, self.config.bash_timeout).await?);
                }
            }
            // All tool results go back in a single user message.
            self.messages
                .push(json!({"role": "user", "content": results}));
        }
    }

    async fn call(&self) -> Result<Value> {
        let session = format!("mitten-{}", self.conversation_id);
        send(
            &self.http,
            &self.config,
            &session,
            &self.system,
            &self.messages,
        )
        .await
    }
}

/// Sends one tiny request to check the key and model work; returns the model's reply.
pub async fn ping(config: &Config) -> Result<String> {
    let messages = [json!({"role": "user", "content": "Reply with the single word OK."})];
    let response = send(
        &http_client()?,
        config,
        "mitten-ping",
        "You are a connectivity check.",
        &messages,
    )
    .await?;
    let text = response["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    Ok(text)
}

fn http_client() -> Result<reqwest::Client> {
    // OpenCode Go asks clients to name themselves instead of sending a generic HTTP-library agent.
    reqwest::Client::builder()
        .user_agent(concat!("mitten/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build HTTP client")
}

/// Calls OpenCode Go's Messages endpoint. `session` identifies the conversation for routing and caching.
#[tracing::instrument(skip_all, fields(model = %config.model, messages = messages.len()))]
async fn send(
    http: &reqwest::Client,
    config: &Config,
    session: &str,
    system: &str,
    messages: &[Value],
) -> Result<Value> {
    let body = json!({
        "model": config.model,
        "max_tokens": config.max_tokens,
        "system": system,
        "messages": messages,
        "tools": [{
            "name": "bash",
            "description": "Run a bash command on the user's machine and return stdout, stderr, and the exit status.",
            "input_schema": {
                "type": "object",
                "properties": {"command": {"type": "string", "description": "The bash command to run."}},
                "required": ["command"],
            },
        }],
    });
    let request = http
        .post(&config.messages_url)
        .header("anthropic-version", "2023-06-01")
        // The Anthropic-format endpoint reads `x-api-key`; a Bearer token gets "Missing API key".
        .header("x-api-key", config.api_key.as_str())
        .header("x-opencode-session", session);

    let response = request
        .json(&body)
        .send()
        .await
        .context("failed to reach the model API")?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("failed to read API response")?;
    if !status.is_success() {
        bail!("model API returned {status}: {text}");
    }
    tracing::debug!(%status, "API call succeeded");
    serde_json::from_str(&text).context("failed to parse API response")
}

/// Executes one tool call after asking the user, and returns its `tool_result` block.
async fn run_tool(block: &Value, io: &mut impl Io, bash_timeout: Duration) -> Result<Value> {
    let id = block["id"].as_str().unwrap_or_default();
    let tool_input = &block["input"];

    let (output, is_error) = match block["name"].as_str().unwrap_or_default() {
        // ponytail: each command runs in a fresh shell; keep a persistent shell if state across calls matters.
        "bash" => match tool_input["command"].as_str() {
            Some(command) if io.confirm(command).await? => execute(command, bash_timeout).await,
            Some(_) => ("the user denied this command".to_owned(), true),
            None => ("missing `command` in bash tool input".to_owned(), true),
        },
        other => (format!("unknown tool `{other}`"), true),
    };

    Ok(json!({
        "type": "tool_result",
        "tool_use_id": id,
        "content": output,
        "is_error": is_error,
    }))
}

async fn execute(command: &str, timeout: Duration) -> (String, bool) {
    let child = Command::new("bash")
        .arg("-c")
        .arg(command)
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(timeout, child).await {
        Err(_) => (format!("command timed out after {timeout:?}"), true),
        Ok(Err(err)) => (format!("failed to start bash: {err}"), true),
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            if !output.status.success() {
                text.push_str(&format!("\n[exit status: {}]", output.status));
            }
            (truncate(text), !output.status.success())
        }
    }
}

fn truncate(mut text: String) -> String {
    if let Some((cut, _)) = text.char_indices().nth(MAX_OUTPUT_CHARS) {
        text.truncate(cut);
        text.push_str("\n[output truncated]");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_output_and_cuts_long_output() {
        assert_eq!(truncate("ok".to_owned()), "ok");
        let long = "é".repeat(MAX_OUTPUT_CHARS + 5);
        let cut = truncate(long);
        assert!(cut.ends_with("[output truncated]"));
        assert_eq!(cut.chars().filter(|c| *c == 'é').count(), MAX_OUTPUT_CHARS);
    }

    #[tokio::test]
    async fn execute_reports_failure_and_captures_stderr() {
        let (out, is_error) =
            execute("echo hi; echo oops >&2; exit 3", Duration::from_secs(5)).await;
        assert!(is_error);
        assert!(out.contains("hi") && out.contains("oops"));
    }
}

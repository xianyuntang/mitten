//! Agent loop over OpenCode Go via Rig's provider clients, with a user-approved bash tool.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::header::{HeaderMap, HeaderValue};
use rig_core::client::CompletionClient;
use rig_core::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, FinishReason,
    ToolDefinition,
};
use rig_core::message::{AssistantContent, Message, ToolCall, ToolResultContent, UserContent};
use rig_core::providers::{anthropic, openai};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::config::{Api, Config};
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

type AnthropicModel = <anthropic::Client as CompletionClient>::CompletionModel;
type OpenaiModel = <openai::CompletionsClient as CompletionClient>::CompletionModel;

/// The configured model behind whichever OpenCode Go endpoint it speaks.
enum Model {
    Anthropic(AnthropicModel),
    Openai(OpenaiModel),
}

impl Model {
    /// `session` goes out as `x-opencode-session` so OpenCode Go can route and cache per conversation.
    fn new(config: &Config, session: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-opencode-session",
            HeaderValue::from_str(session).context("invalid session id")?,
        );
        // OpenCode Go asks clients to name themselves instead of sending a generic HTTP-library agent.
        let http = reqwest::Client::builder()
            .user_agent(concat!("mitten/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build HTTP client")?;
        let key = config.api_key.as_str();
        Ok(match config.api {
            Api::Anthropic => Self::Anthropic(
                anthropic::Client::builder()
                    .api_key(key)
                    .base_url(&config.base_url)
                    .http_client(http)
                    .http_headers(headers)
                    .build()
                    .context("failed to build Anthropic-format client")?
                    .completion_model(&config.model),
            ),
            Api::Openai => Self::Openai(
                openai::Client::builder()
                    .api_key(key)
                    .base_url(&config.base_url)
                    .http_client(http)
                    .http_headers(headers)
                    .build()
                    .context("failed to build OpenAI-format client")?
                    .completions_api()
                    .completion_model(&config.model),
            ),
        })
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        match self {
            Self::Anthropic(model) => model.completion(request).await,
            Self::Openai(model) => model.completion(request).await,
        }
    }
}

impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Anthropic(_) => "Model::Anthropic",
            Self::Openai(_) => "Model::Openai",
        })
    }
}

fn bash_tool() -> ToolDefinition {
    ToolDefinition {
        name: "bash".to_owned(),
        description: "Run a bash command on the user's machine and return stdout, stderr, and the exit status.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {"command": {"type": "string", "description": "The bash command to run."}},
            "required": ["command"],
        }),
    }
}

fn request(config: &Config, system: &str, messages: &[Message]) -> CompletionRequest {
    CompletionRequest {
        model: None,
        preamble: None,
        chat_history: std::iter::once(Message::system(system))
            .chain(messages.iter().cloned())
            .collect(),
        documents: Vec::new(),
        tools: vec![bash_tool()],
        temperature: None,
        max_tokens: Some(u64::from(config.max_tokens)),
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    }
}

/// One conversation, kept in memory for the process lifetime.
#[derive(Debug)]
pub struct Agent {
    model: Model,
    config: Config,
    db: Db,
    conversation_id: i64,
    system: String,
    messages: Vec<Message>,
}

impl Agent {
    /// Resumes the stored conversation named `key` (e.g. `terminal`, `discord:<channel>`).
    pub async fn new(config: Config, db: Db, key: &str) -> Result<Self> {
        let conversation = db.conversation(key).await?;
        let messages = db
            .messages(conversation.id)
            .await?
            .into_iter()
            .map(|row| serde_json::from_value(row.content))
            .collect::<Result<Vec<Message>, _>>()
            .context("stored conversation is unreadable; send /new to start over")?;
        let cwd = std::env::current_dir().context("failed to read current directory")?;
        let system = format!(
            "You are Mitten, a personal agent running on the user's machine ({os}). \
             Use the bash tool to inspect and change things; the user approves each command. \
             Working directory: {cwd}.",
            os = std::env::consts::OS,
            cwd = cwd.display(),
        );
        Ok(Self {
            model: Model::new(&config, &format!("mitten-{}", conversation.id))?,
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
        let turn = self.messages[turn_start..]
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<Value>, _>>()?;
        self.db
            .append(self.conversation_id, turn)
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
        self.messages.push(Message::user(prompt));
        loop {
            let response = self
                .model
                .complete(request(&self.config, &self.system, &self.messages))
                .await
                .context("model request failed")?;
            match response.finish_reason() {
                Some(FinishReason::Length) => {
                    bail!("response hit max_tokens ({})", self.config.max_tokens)
                }
                Some(FinishReason::ContentFilter) => bail!("the provider filtered this response"),
                _ => {}
            }
            if response.choice.is_empty() {
                bail!("the model returned an empty response");
            }

            let mut calls: Vec<ToolCall> = Vec::new();
            for content in &response.choice {
                match content {
                    AssistantContent::Text(text) if !text.text().trim().is_empty() => {
                        io.say(text.text()).await?;
                    }
                    AssistantContent::ToolCall(call) => calls.push(call.clone()),
                    _ => {}
                }
            }
            // Keep the full content (reasoning included) so the provider can replay it.
            self.messages.push(Message::Assistant {
                id: response.message_id.clone(),
                content: response.choice,
            });
            if calls.is_empty() {
                return Ok(());
            }

            let mut results = Vec::new();
            for call in &calls {
                let output = run_tool(call, io, self.config.bash_timeout).await?;
                results.push(UserContent::tool_result_for(
                    call.id.clone(),
                    call.provider.clone(),
                    call.function.name.clone(),
                    vec![ToolResultContent::text(output)],
                ));
            }
            // All tool results go back in a single user message.
            self.messages.push(Message::User { content: results });
        }
    }
}

/// Sends one tiny request to check the key and model work; returns the model's reply.
pub async fn ping(config: &Config) -> Result<String> {
    let model = Model::new(config, "mitten-ping")?;
    let messages = [Message::user("Reply with the single word OK.")];
    let response = model
        .complete(request(config, "You are a connectivity check.", &messages))
        .await?;
    Ok(response
        .choice
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text()),
            _ => None,
        })
        .collect())
}

/// Runs one tool call after asking the user; failures are reported to the model as text.
async fn run_tool(call: &ToolCall, io: &mut impl Io, bash_timeout: Duration) -> Result<String> {
    if call.function.name != "bash" {
        return Ok(format!("error: unknown tool `{}`", call.function.name));
    }
    // ponytail: each command runs in a fresh shell; keep a persistent shell if state across calls matters.
    let Some(command) = call.function.arguments["command"].as_str() else {
        return Ok("error: missing `command` in bash tool input".to_owned());
    };
    if !io.confirm(command).await? {
        return Ok("error: the user denied this command".to_owned());
    }
    let (output, is_error) = execute(command, bash_timeout).await;
    Ok(if is_error {
        format!("error:\n{output}")
    } else {
        output
    })
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

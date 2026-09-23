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
use crate::memory;

// ponytail: hard cap on tool output fed back to the model; summarize or page if it bites.
const MAX_OUTPUT_CHARS: usize = 30_000;

/// Where the agent talks to the user: the terminal or a chat channel.
pub trait Io {
    /// Shows model text to the user.
    async fn say(&mut self, text: &str) -> Result<()>;
    /// Asks the user to approve `command`; anything but an explicit yes is a no.
    async fn confirm(&mut self, command: &str) -> Result<bool>;
    /// Tells the user about something the agent did on its own, like saving a memory.
    async fn note(&mut self, text: &str) -> Result<()> {
        self.say(text).await
    }
    /// Reports what an approved command printed and whether it succeeded.
    async fn ran(&mut self, _output: &str, _ok: bool) -> Result<()> {
        Ok(())
    }
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

const IDENTITY: &str = "\
You are Mitten, a personal agent running on the user's machine. \
Be direct: match reply length to the ask. Finished work gets a short report of what changed, \
what's verified, and what's left, never a replay of the process. No filler, no restating the request. \
When unsure, say so plainly.

# Tools
Use the bash tool to act; don't describe what you would do. If you say you'll do something, \
make the call in the same response. Keep going until the task is done and verified.
Never answer from memory what a command can tell you: time and date, arithmetic, hashes, file contents, \
system state (OS, disk, ports, processes), git state.
The user approves every command. If one is denied, don't retry it or a variant; ask what they want instead.
Prefer non-interactive flags (-y, --no-pager). Each command runs in a fresh shell with a timeout, \
so cd and exported variables do not persist.
If something fails and blocks you, say so and try another route. Never fabricate output.
When the obvious interpretation is clear, act; ask only when the ambiguity changes what you would run.";

/// Stable text first, per-session details last, so the provider can cache the prefix.
/// Memory is read once here, so edits show up from the next conversation (or `/new`) on.
async fn system_prompt(db: &Db, conversation_id: i64, key: &str) -> Result<String> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let memories = db
        .memories(conversation_id)
        .await
        .context("failed to load memory")?;
    Ok(format!(
        "{IDENTITY}\n\n{guidance}\n\n{hint}\n\n{saved}\n\nOS: {os}. Working directory: {cwd}.",
        guidance = memory::GUIDANCE,
        hint = platform_hint(key),
        saved = memory::snapshot(&memories),
        os = os_name(),
        cwd = cwd.display(),
    ))
}

/// Formatting guidance for where the conversation lives, keyed like `Agent::new`.
fn platform_hint(key: &str) -> &'static str {
    if key.starts_with("discord:") {
        "You are chatting over Discord. Markdown renders; tables do not, use bullets. Keep replies short."
    } else {
        "You are in a plain terminal. Markdown does not render; write plain text."
    }
}

/// End of the last complete tool round in the turn starting at `turn_start`, or `turn_start` if none.
/// Every user message after the turn's prompt carries tool results, and each closes a round.
fn completed_rounds_end(messages: &[Message], turn_start: usize) -> usize {
    messages
        .iter()
        .enumerate()
        .skip(turn_start + 1)
        .rev()
        .find(|(_, message)| matches!(message, Message::User { .. }))
        .map_or(turn_start, |(index, _)| index + 1)
}

/// The distro name on Linux (e.g. `Ubuntu 24.04 LTS`) so the model picks the right package
/// manager; the bare OS name elsewhere.
fn os_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| pretty_name(&text))
        .unwrap_or_else(|| std::env::consts::OS.to_owned())
}

fn pretty_name(os_release: &str) -> Option<String> {
    os_release.lines().find_map(|line| {
        let value = line.strip_prefix("PRETTY_NAME=")?.trim_matches(['"', '\'']);
        (!value.is_empty()).then(|| value.to_owned())
    })
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
        tools: vec![bash_tool(), memory::tool()],
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
    key: String,
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
        let system = system_prompt(&db, conversation.id, key).await?;
        Ok(Self {
            model: Model::new(&config, &format!("mitten-{}", conversation.id))?,
            config,
            db,
            conversation_id: conversation.id,
            key: key.to_owned(),
            system,
            messages,
        })
    }

    pub fn describe(&self) -> String {
        format!("opencode-go / {}", self.config.model)
    }

    /// Validates and applies one memory tool call; problems go back to the model as text.
    async fn remember(&self, args: &Value, io: &mut impl Io) -> Result<String> {
        let entries = self.db.memories(self.conversation_id).await?;
        let (edit, used) = match memory::plan(&entries, args) {
            Ok(planned) => planned,
            Err(problem) => return Ok(format!("error: {problem}")),
        };
        let note = memory::describe(&edit, &entries);
        self.db.save_memory(self.conversation_id, edit).await?;
        io.note(&note).await?;
        Ok(format!(
            "saved; memory uses {used}/{} characters. It loads into the next conversation.",
            memory::CHAR_LIMIT
        ))
    }

    /// Whether this conversation picked up stored history.
    pub fn is_resumed(&self) -> bool {
        !self.messages.is_empty()
    }

    /// Runs one user turn to completion, looping while the model calls tools, then saves it.
    /// On failure the turn is cut back to its last complete tool round so the history stays valid;
    /// rounds whose commands already ran are kept, so the model knows what changed.
    pub async fn run_turn(&mut self, prompt: &str, io: &mut impl Io) -> Result<()> {
        let turn_start = self.messages.len();
        let result = self.drive(prompt, io).await;
        if let Err(err) = &result {
            let kept = completed_rounds_end(&self.messages, turn_start);
            self.messages.truncate(kept);
            if kept == turn_start {
                return result;
            }
            self.messages
                .push(Message::assistant(format!("[turn aborted: {err:#}]")));
        }
        let turn = self.messages[turn_start..]
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<Value>, _>>()?;
        self.db
            .append(self.conversation_id, turn)
            .await
            .context("failed to save the conversation")?;
        result
    }

    /// Forgets the conversation, in memory and on disk.
    pub async fn reset(&mut self) -> Result<()> {
        self.db.clear(self.conversation_id).await?;
        self.messages.clear();
        // A fresh conversation picks up memory saved since the last one started.
        self.system = system_prompt(&self.db, self.conversation_id, &self.key).await?;
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
                let output = match call.function.name.as_str() {
                    "bash" => run_bash(call, io, self.config.bash_timeout).await?,
                    "memory" => self.remember(&call.function.arguments, io).await?,
                    other => format!("error: unknown tool `{other}`"),
                };
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

/// Runs one bash call after asking the user; failures are reported to the model as text.
async fn run_bash(call: &ToolCall, io: &mut impl Io, bash_timeout: Duration) -> Result<String> {
    // ponytail: each command runs in a fresh shell; keep a persistent shell if state across calls matters.
    let Some(command) = call.function.arguments["command"].as_str() else {
        return Ok("error: missing `command` in bash tool input".to_owned());
    };
    if !io.confirm(command).await? {
        return Ok("error: the user denied this command".to_owned());
    }
    let (output, is_error) = execute(command, bash_timeout).await;
    io.ran(&output, !is_error).await?;
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

    #[test]
    fn pretty_name_reads_os_release() {
        let text = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04 LTS\"\nID=ubuntu\n";
        assert_eq!(pretty_name(text).as_deref(), Some("Ubuntu 24.04 LTS"));
        assert_eq!(pretty_name("ID=alpine\n"), None);
    }

    #[test]
    fn completed_rounds_end_keeps_only_finished_tool_rounds() {
        let prior = [Message::user("old"), Message::assistant("old reply")];
        let mut messages = prior.to_vec();
        messages.push(Message::user("prompt"));
        assert_eq!(completed_rounds_end(&messages, 2), 2);
        messages.push(Message::assistant("calling bash"));
        messages.push(Message::user("tool results"));
        messages.push(Message::assistant("calling bash again"));
        assert_eq!(completed_rounds_end(&messages, 2), 5);
    }

    #[tokio::test]
    async fn execute_reports_failure_and_captures_stderr() {
        let (out, is_error) =
            execute("echo hi; echo oops >&2; exit 3", Duration::from_secs(5)).await;
        assert!(is_error);
        assert!(out.contains("hi") && out.contains("oops"));
    }
}

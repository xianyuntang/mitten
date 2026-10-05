//! Agent loop over OpenCode Go via Rig's provider clients, with read-only local tools.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::header::{HeaderMap, HeaderValue};
use rig_core::client::CompletionClient;
use rig_core::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, FinishReason,
    ToolDefinition,
};
use rig_core::message::{
    AssistantContent, Image, Message, ToolCall, ToolResultContent, UserContent,
};
use rig_core::providers::{anthropic, openai};
use serde_json::Value;

use crate::claude_code;
use crate::compact;
use crate::config::{Api, Config};
use crate::cron;
use crate::db::Db;
use crate::fetch::{self, Fetcher};
use crate::files;
use crate::mcp::Mcp;
use crate::memory;
use crate::review::{self, Guard, Reviewer};
use crate::search;
use crate::search::WebSearch;
use crate::settings;

/// How long a new model gets to answer its test request before a switch is refused.
const PING_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the agent talks to the user: the terminal or a chat channel.
pub trait Io {
    /// Shows model text to the user.
    async fn say(&mut self, text: &str) -> Result<()>;
    /// Asks the user to approve `action`; anything but an explicit yes is a no.
    async fn confirm(&mut self, action: &str) -> Result<bool>;
    /// Tells the user about something the agent did on its own, like saving a memory.
    async fn note(&mut self, text: &str) -> Result<()> {
        self.say(text).await
    }
    /// Reports how an approved action went.
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
                    .completion_model(&config.model)
                    // Anthropic-format providers only cache what is marked; OpenAI-format ones cache
                    // the longest shared prefix on their own.
                    .with_automatic_caching(),
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

/// The reviewer model for auto approval, with the settings to call it.
#[derive(Debug)]
struct Review {
    model: Model,
    config: Config,
}

impl Review {
    /// `None` unless `config` asks for auto approval.
    fn new(config: &Config, session: &str) -> Result<Option<Arc<Self>>> {
        let crate::config::Approval::Auto { model } = &config.approval else {
            return Ok(None);
        };
        let config = if *model == config.model {
            config.clone()
        } else {
            Config {
                model: model.clone(),
                api: Api::for_model(model),
                ..config.clone()
            }
        };
        let model = Model::new(&config, session)?;
        Ok(Some(Arc::new(Self { model, config })))
    }
}

impl Reviewer for Review {
    async fn review(&self, prompt: String) -> Result<String> {
        let mut request = request(&self.config, review::SYSTEM, &[Message::user(prompt)], &[]);
        request.tools.clear();
        let response = self
            .model
            .complete(request)
            .await
            .context("review request failed")?;
        Ok(response
            .choice
            .iter()
            .filter_map(|content| match content {
                AssistantContent::Text(text) => Some(text.text()),
                _ => None,
            })
            .collect())
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
Your built-in tools only read; you cannot run commands. Tools named <server>__<tool> come from MCP \
servers the user connected and may act; the user may be asked to approve those calls, and if one is \
denied, don't retry it or a variant. If you say you'll do something, make the call in the same response.
Each user message starts with the time it was sent, like [sent 2026-01-02 09:00:00 CST ...]; \
take the date and time from there, never from your own sense of today. Never answer from memory \
what a tool can tell you, like file contents (read_file, list_dir). If a task needs a command run or a file changed, say what to run and let the \
user do it.
Hidden paths (starting with .) are refused; don't try to get around that.
If something fails and blocks you, say so and try another route. Never fabricate output.
When the obvious interpretation is clear, act; ask only when the ambiguity changes what you would run.";

/// Most stable text first and memory last, so the provider can cache the prefix: a memory edit
/// only changes the tail. Rebuilt before every turn, so memory saved in any conversation shows up
/// on the next message everywhere; the text only changes when memory or settings do.
/// Web search and Claude Code guidance are added when those tools are configured.
/// `user` picks whose memory loads: a Discord user ID, or `None` for the terminal.
async fn system_prompt(db: &Db, key: &str, config: &Config, user: Option<u64>) -> Result<String> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let memories = db.memories(user).await.context("failed to load memory")?;
    let mut tools = if config.searxng.is_some() {
        format!("{}\n\n", search::GUIDANCE)
    } else {
        String::new()
    };
    if let Some(claude_code) = &config.claude_code {
        tools.push_str(&format!("{}\n\n", claude_code::guidance(claude_code)));
    }
    Ok(format!(
        "{IDENTITY}\n\n{tools}{guidance}\n\nOS: {os}. Working directory: {cwd}.\n\n{hint}\n\n{saved}",
        guidance = memory::GUIDANCE,
        hint = platform_hint(key),
        saved = memory::snapshot(&memories),
        os = os_name(),
        cwd = cwd.display(),
    ))
}

/// Formatting guidance for where the conversation lives, keyed like `Agent::new`.
/// Scheduled jobs (`cron:<id>`) post to Discord too.
fn platform_hint(key: &str) -> &'static str {
    if key.starts_with("discord:") || key.starts_with("cron:") {
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

/// `text` as a Markdown quote, so a job's instructions stand apart from the note above them.
fn quote(text: &str) -> String {
    text.lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The current time in `zone`, e.g. `2026-10-01 09:00:00 CST Asia/Taipei (UTC+0800), Thursday`.
fn now(zone: chrono_tz::Tz) -> String {
    chrono::Utc::now()
        .with_timezone(&zone)
        .format(&format!("%Y-%m-%d %H:%M:%S %Z {zone} (UTC%z), %A"))
        .to_string()
}

/// `extra` holds tools beyond the built-in ones, i.e. MCP tools.
fn request(
    config: &Config,
    system: &str,
    messages: &[Message],
    extra: &[ToolDefinition],
) -> CompletionRequest {
    CompletionRequest {
        model: None,
        preamble: None,
        chat_history: std::iter::once(Message::system(system))
            .chain(messages.iter().cloned())
            .collect(),
        documents: Vec::new(),
        tools: [
            files::read_tool(),
            files::list_tool(),
            memory::tool(),
            fetch::tool(),
            settings::tool(),
        ]
        .into_iter()
        .chain(config.searxng.is_some().then(search::tool))
        .chain(config.claude_code.as_ref().map(claude_code::tool))
        .chain(extra.iter().cloned())
        .collect(),
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
    /// Set when `[approval] mode = "auto"`.
    reviewer: Option<Arc<Review>>,
    config: Config,
    db: Db,
    conversation_id: i64,
    /// Set when `[tools.searxng]` is configured.
    search: Option<WebSearch>,
    fetcher: Fetcher,
    /// Shared by every conversation in the process.
    mcp: Arc<Mcp>,
    key: String,
    /// Who sent the current turn: a Discord user ID, or `None` for the terminal. Picks the memory.
    user: Option<u64>,
    system: String,
    messages: Vec<Message>,
}

impl Agent {
    /// Resumes the stored conversation named `key` (e.g. `terminal`, `discord:<channel>`).
    pub async fn new(config: Config, db: Db, mcp: Arc<Mcp>, key: &str) -> Result<Self> {
        let conversation = db.conversation(key).await?;
        let messages = db
            .messages(conversation.id)
            .await?
            .into_iter()
            .map(|row| serde_json::from_value(row.content))
            .collect::<Result<Vec<Message>, _>>()
            .context("stored conversation is unreadable; send /new to start over")?;
        let system = system_prompt(&db, key, &config, None).await?;
        let search = config.searxng.clone().map(WebSearch::new).transpose()?;
        Ok(Self {
            model: Model::new(&config, &format!("mitten-{}", conversation.id))?,
            reviewer: Review::new(&config, &format!("mitten-{}-review", conversation.id))?,
            config,
            db,
            conversation_id: conversation.id,
            search,
            fetcher: Fetcher::new()?,
            mcp,
            key: key.to_owned(),
            user: None,
            system,
            messages,
        })
    }

    pub fn describe(&self) -> String {
        format!("opencode-go / {}", self.config.model)
    }

    /// Validates and applies one memory tool call; problems go back to the model as text.
    async fn remember(&self, args: &Value, io: &mut impl Io) -> Result<String> {
        let entries = self.db.memories(self.user).await?;
        let (edit, used) = match memory::plan(&entries, args) {
            Ok(planned) => planned,
            Err(problem) => return Ok(format!("error: {problem}")),
        };
        let note = memory::describe(&edit, &entries);
        self.db.save_memory(self.user, edit).await?;
        io.note(&note).await?;
        Ok(format!(
            "saved; memory uses {used}/{} characters. Every conversation with this person sees it from its next message.",
            memory::CHAR_LIMIT
        ))
    }

    /// Whether this conversation may schedule jobs: only Discord ones, since `mitten serve` runs
    /// them and posts back there. Job runs themselves can't schedule more.
    fn schedules(&self) -> bool {
        self.key.starts_with("discord:")
    }

    /// Runs one cron tool call; problems go back to the model as text.
    async fn cron(&self, args: &Value, io: &mut impl Io) -> Result<String> {
        let zone = self.config.timezone;
        match args["action"].as_str() {
            Some("list") => Ok(cron::list(&self.db.jobs().await?, zone)),
            Some("add") => {
                let job = match cron::plan(args, chrono::Utc::now().with_timezone(&zone)) {
                    Ok(job) => job,
                    Err(problem) => return Ok(format!("error: {problem}")),
                };
                let when = cron::when(job.schedule.as_deref(), job.next_run, zone);
                let name = job.name.clone();
                let prompt = quote(&job.prompt);
                let id = self.db.add_job(self.key.clone(), self.user, job).await?;
                let note = format!("⏰ scheduled #{id} {name} ({when})\n{prompt}");
                // A full message, not a note: notes show one line, and the user should see the
                // instructions the job will run with.
                io.say(&note).await?;
                Ok(note)
            }
            Some("update") => {
                let jobs = self.db.jobs().await?;
                let job = match cron::target(&jobs, args, zone).and_then(|job| {
                    cron::revise(job, args, chrono::Utc::now().with_timezone(&zone))
                }) {
                    Ok(job) => job,
                    Err(problem) => return Ok(format!("error: {problem}")),
                };
                if !self.db.update_job(&job).await? {
                    return Ok(format!("error: no job #{}", job.id));
                }
                let when = cron::when(job.schedule.as_deref(), job.next_run, zone);
                let note = format!(
                    "⏰ updated #{} {} ({when})\n{}",
                    job.id,
                    job.name,
                    quote(&job.prompt)
                );
                io.say(&note).await?;
                Ok(note)
            }
            Some("run") => {
                let jobs = self.db.jobs().await?;
                let job = match cron::target(&jobs, args, zone) {
                    Ok(job) => job,
                    Err(problem) => return Ok(format!("error: {problem}")),
                };
                let trial = cron::trial(job, chrono::Utc::now().timestamp());
                let id = self.db.add_job(job.target.clone(), job.user, trial).await?;
                let note = format!(
                    "⏰ trial run of #{} {} starts within a minute as #{id}; its reply posts where \
                     the job was made",
                    job.id, job.name
                );
                io.note(&note).await?;
                Ok(note)
            }
            Some("remove") => {
                let jobs = self.db.jobs().await?;
                let (id, name) = match cron::target(&jobs, args, zone) {
                    Ok(job) => (job.id, job.name.clone()),
                    Err(problem) => return Ok(format!("error: {problem}")),
                };
                if !self.db.remove_job(id).await? {
                    return Ok(format!("error: no job #{id}"));
                }
                let note = format!("⏰ removed #{id} {name}");
                io.note(&note).await?;
                Ok(note)
            }
            other => Ok(format!(
                "error: unknown action {other:?}; use add, list, remove, update, or run"
            )),
        }
    }

    /// Runs one search and tells the user what was searched.
    async fn web_search(&self, args: &Value, io: &mut impl Io) -> Result<String> {
        let Some(search) = &self.search else {
            return Ok("error: web search is not configured".to_owned());
        };
        if let Some(query) = args["query"].as_str() {
            io.note(&format!("🔎 searching: {query}")).await?;
        }
        Ok(search.run(args).await)
    }

    /// Runs one settings tool call; a new model must answer a test request before it is saved.
    async fn settings(&mut self, args: &Value, io: &mut impl Io) -> Result<String> {
        match args["action"].as_str() {
            Some("get") => return Ok(settings::show(&self.config)),
            Some("set") => {}
            _ => return Ok("error: action must be `get` or `set`".to_owned()),
        }
        let key = args["key"].as_str().unwrap_or_default();
        let path = self.config.path.clone();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => return Ok(format!("error: cannot read {}: {err}", path.display())),
        };
        let (text, fresh) = match settings::edit(&text, key, &args["value"]) {
            Ok(edited) => edited,
            Err(err) => return Ok(format!("error: {err:#}")),
        };
        let value = args["value"].to_string();
        if !io
            .confirm(&format!("set {key} = {value} in {}", path.display()))
            .await?
        {
            return Ok("error: the user denied this change".to_owned());
        }
        if fresh.model != self.config.model {
            let ping = tokio::time::timeout(PING_TIMEOUT, ping(&fresh)).await;
            if let Some(err) = match ping {
                Err(_) => Some(format!("no reply in {PING_TIMEOUT:?}")),
                Ok(Err(err)) => Some(format!("{err:#}")),
                Ok(Ok(_)) => None,
            } {
                let output = format!("{} did not answer a test request: {err}", fresh.model);
                io.ran(&output, false).await?;
                return Ok(format!("error: {output}; nothing was saved"));
            }
        }
        if let Err(err) = crate::config::write(&path, &text) {
            io.ran(&format!("{err:#}"), false).await?;
            return Ok(format!("error: {err:#}"));
        }
        io.ran("saved", true).await?;
        self.apply(fresh)?;
        Ok(format!(
            "saved; {key} = {value} applies from the next request"
        ))
    }

    /// Takes model and tool settings from `fresh`, rebuilding the client if the endpoint changed.
    /// Web search, Discord, and storage keep their startup values until restart.
    fn apply(&mut self, fresh: Config) -> Result<()> {
        let old = &self.config;
        if fresh.model != old.model
            || fresh.api != old.api
            || fresh.base_url != old.base_url
            || fresh.api_key.as_str() != old.api_key.as_str()
        {
            self.model = Model::new(&fresh, &format!("mitten-{}", self.conversation_id))?;
        }
        let endpoint_changed =
            fresh.base_url != old.base_url || fresh.api_key.as_str() != old.api_key.as_str();
        if fresh.approval != old.approval || endpoint_changed {
            self.reviewer =
                Review::new(&fresh, &format!("mitten-{}-review", self.conversation_id))?;
        }
        let searxng = self.config.searxng.take();
        self.config = Config { searxng, ..fresh };
        Ok(())
    }

    /// Picks up config edits made elsewhere: another conversation, `mitten configure`, or by hand.
    /// A file that no longer loads is logged and the current settings stay.
    fn reload(&mut self) -> Result<()> {
        match Config::load(&self.config.path) {
            Ok(fresh) => self.apply(fresh),
            Err(err) => {
                tracing::warn!("keeping current settings: {err:#}");
                Ok(())
            }
        }
    }

    /// Whether this conversation picked up stored history.
    pub fn is_resumed(&self) -> bool {
        !self.messages.is_empty()
    }

    /// Runs one user turn to completion, looping while the model calls tools, then saves it.
    /// On failure the turn is cut back to its last complete tool round so the history stays valid;
    /// rounds whose commands already ran are kept, so the model knows what changed.
    /// `images` go to the model alongside `prompt`. `user` sent it: a Discord user ID, or `None`
    /// for the terminal; their memory is the one loaded and edited.
    pub async fn run_turn(
        &mut self,
        prompt: &str,
        images: Vec<Image>,
        user: Option<u64>,
        io: &mut impl Io,
    ) -> Result<()> {
        self.reload()?;
        self.user = user;
        self.system = system_prompt(&self.db, &self.key, &self.config, user).await?;
        self.compact_if_needed(prompt, io).await?;
        let turn_start = self.messages.len();
        let result = match self.reviewer.clone() {
            Some(reviewer) => {
                let mut io = Guard::new(io, reviewer.as_ref(), prompt);
                self.drive(prompt, images, &mut io).await
            }
            None => self.drive(prompt, images, io).await,
        };
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

    /// Summarizes older turns once history (plus `prompt`) passes the configured budget, keeping
    /// recent whole turns verbatim. A failed summary is reported and the turn goes ahead uncompacted.
    async fn compact_if_needed(&mut self, prompt: &str, io: &mut impl Io) -> Result<()> {
        let budget = self.config.compact_at_tokens;
        if compact::estimate_tokens(&self.messages) + prompt.chars().count() / 3 <= budget {
            return Ok(());
        }
        let cut = compact::split_point(&self.messages, budget / 4);
        if cut == 0 {
            return Ok(());
        }
        io.note("🗜️ compacting earlier conversation…").await?;
        let mut request = request(
            &self.config,
            compact::SYSTEM,
            &[Message::user(compact::transcript(&self.messages[..cut]))],
            &[],
        );
        request.tools.clear();
        let summary = match self.model.complete(request).await {
            Ok(response) => response
                .choice
                .iter()
                .filter_map(|content| match content {
                    AssistantContent::Text(text) => Some(text.text()),
                    _ => None,
                })
                .collect::<String>(),
            Err(err) => {
                tracing::warn!("compaction failed: {err:#}");
                return io
                    .note("🗜️ compaction failed; continuing with full history")
                    .await;
            }
        };
        if summary.trim().is_empty() {
            return io
                .note("🗜️ compaction returned nothing; continuing with full history")
                .await;
        }
        let mut compacted = compact::summary_messages(&summary).to_vec();
        compacted.extend(self.messages.drain(cut..));
        let rows = compacted
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<Value>, _>>()?;
        self.db
            .rewrite(self.conversation_id, rows)
            .await
            .context("failed to save the compacted conversation")?;
        self.messages = compacted;
        Ok(())
    }

    /// Forgets the conversation, in memory and on disk.
    pub async fn reset(&mut self) -> Result<()> {
        self.db.clear(self.conversation_id).await?;
        self.messages.clear();
        Ok(())
    }

    async fn drive(&mut self, prompt: &str, images: Vec<Image>, io: &mut impl Io) -> Result<()> {
        // Models skip a clock tool and guess the date, so every message carries its send time.
        let sent = format!("[sent {}]", now(self.config.timezone));
        let text = if prompt.is_empty() {
            sent
        } else {
            format!("{sent}\n{prompt}")
        };
        let content = std::iter::once(UserContent::text(text))
            .chain(images.into_iter().map(UserContent::Image))
            .collect();
        self.messages.push(Message::User { content });
        let extra: Vec<ToolDefinition> = self
            .schedules()
            .then(cron::tool)
            .into_iter()
            .chain(self.mcp.tools().iter().cloned())
            .collect();
        loop {
            let response = self
                .model
                .complete(request(&self.config, &self.system, &self.messages, &extra))
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
                    "claude_code" => match &self.config.claude_code {
                        Some(config) => {
                            claude_code::run(config, &call.function.arguments, io).await?
                        }
                        None => "error: claude_code is not configured".to_owned(),
                    },
                    "read_file" => {
                        let args = &call.function.arguments;
                        if let Some(path) = args["path"].as_str() {
                            io.note(&format!("📄 reading: {path}")).await?;
                        }
                        files::read(&self.config, args).await
                    }
                    "list_dir" => files::list(&self.config, &call.function.arguments).await,
                    "memory" => self.remember(&call.function.arguments, io).await?,
                    "cron" if self.schedules() => self.cron(&call.function.arguments, io).await?,
                    "settings" => self.settings(&call.function.arguments, io).await?,
                    "fetch_url" => {
                        let args = &call.function.arguments;
                        if let Some(url) = args["url"].as_str() {
                            let verb = if args["render"].as_bool().unwrap_or(false) {
                                "rendering"
                            } else {
                                "reading"
                            };
                            io.note(&format!("🌐 {verb}: {url}")).await?;
                        }
                        self.fetcher.run(args).await
                    }
                    "web_search" if self.search.is_some() => {
                        self.web_search(&call.function.arguments, io).await?
                    }
                    other => match self.mcp.call(other, &call.function.arguments, io).await {
                        Some(output) => output?,
                        None => format!("error: unknown tool `{other}`"),
                    },
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
        .complete(request(
            config,
            "You are a connectivity check.",
            &messages,
            &[],
        ))
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Calls the real model from `~/.config/mitten/config.toml`: `cargo test -- --ignored reviewer`.
    #[tokio::test]
    #[ignore = "sends two requests to the configured model"]
    async fn reviewer_passes_reads_and_flags_deletes() {
        let path = std::env::home_dir()
            .expect("home")
            .join(".config/mitten/config.toml");
        let loaded = Config::load(&path).expect("config loads");
        let config = Config {
            approval: crate::config::Approval::Auto {
                model: loaded.model.clone(),
            },
            ..loaded
        };
        let reviewer = Review::new(&config, "mitten-review-test")
            .expect("builds")
            .expect("auto");
        let verdict = |request: &str, action: &str| {
            let prompt = review::prompt(request, action);
            let reviewer = Arc::clone(&reviewer);
            async move { review::parse(&reviewer.review(prompt).await.expect("review runs")) }
        };
        let read = verdict(
            "what issues are assigned to me?",
            "linear → list_issues\n{\"assignee\": \"me\"}",
        )
        .await
        .expect("parses");
        assert!(!read.risky, "{}", read.reason);
        let delete = verdict(
            "what issues are assigned to me?",
            "linear → delete_project\n{\"id\": \"ENG\"}",
        )
        .await
        .expect("parses");
        assert!(delete.risky, "{}", delete.reason);
    }

    #[test]
    fn now_reports_the_date_in_zone() {
        let text = now(chrono_tz::Tz::Asia__Taipei);
        assert!(
            text.starts_with("20") && text.contains("Asia/Taipei (UTC+0800)"),
            "{text}"
        );
    }
}

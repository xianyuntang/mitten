//! TOML config file: OpenCode Go model and key, tools, MCP servers, logging, storage, and Discord.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
const DEFAULT_MODEL: &str = "minimax-m3";
pub const DEFAULT_MAX_TOKENS: u32 = 16_000;
pub const DEFAULT_COMPACT_AT_TOKENS: usize = 200_000;
pub const MIN_COMPACT_AT_TOKENS: usize = 10_000;

/// Which OpenCode Go endpoint format a model speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Api {
    /// `/messages`: MiniMax, Qwen, and Claude.
    Anthropic,
    /// `/chat/completions`: GLM, Kimi, DeepSeek, and the rest.
    Openai,
    /// `/responses`, OpenAI's Responses API: GPT, Grok, and Muse Spark.
    Responses,
    /// Google's `generateContent`: Gemini.
    Gemini,
}

impl Api {
    pub const ALL: [Self; 4] = [Self::Anthropic, Self::Openai, Self::Responses, Self::Gemini];

    /// The name `model.api` takes.
    pub fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
            Self::Responses => "responses",
            Self::Gemini => "gemini",
        }
    }

    /// OpenCode Go serves MiniMax, Qwen, and Claude in Anthropic format and most others as chat completions.
    // ponytail: name-prefix guess; set `model.api` for anything it gets wrong.
    pub fn for_model(model: &str) -> Self {
        if ["minimax", "qwen", "claude"]
            .iter()
            .any(|prefix| model.starts_with(prefix))
        {
            Self::Anthropic
        } else if ["gpt", "grok", "muse"]
            .iter()
            .any(|prefix| model.starts_with(prefix))
        {
            Self::Responses
        } else if model.starts_with("gemini") {
            Self::Gemini
        } else {
            Self::Openai
        }
    }
}

/// API key or token whose `Debug` output is redacted.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    /// IANA name, e.g. `Asia/Taipei`; defaults to this machine's time zone.
    timezone: Option<String>,
    #[serde(default)]
    model: ModelSection,
    #[serde(rename = "opencode-go")]
    opencode_go: OpenCodeGoSection,
    #[serde(default)]
    tools: ToolsSection,
    #[serde(default)]
    mcp: McpSection,
    #[serde(default)]
    approval: ApprovalSection,
    #[serde(default)]
    log: LogSection,
    discord: Option<DiscordSection>,
    #[serde(default)]
    database: DatabaseSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseSection {
    /// Defaults to `~/.local/share/mokuzu/mokuzu.db`.
    path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelSection {
    name: Option<String>,
    /// Wire format; inferred from the model name when omitted.
    api: Option<Api>,
    max_tokens: Option<u32>,
    /// Estimated history size that triggers compaction.
    compact_at_tokens: Option<usize>,
}

/// OpenCode Go serves MiniMax, Qwen, and Claude models through an Anthropic-format Messages endpoint.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenCodeGoSection {
    api_key: Secret,
    base_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsSection {
    /// Left over from the removed bash tool; accepted so older configs still load.
    #[serde(default, rename = "bash")]
    _bash: Option<serde::de::IgnoredAny>,
    /// Left over from the removed claude_code tool; accepted so older configs still load.
    #[serde(default, rename = "claude_code")]
    _claude_code: Option<serde::de::IgnoredAny>,
    searxng: Option<SearxngSection>,
    brave: Option<BraveSection>,
}

/// `[tools.searxng]`: a SearXNG instance for the `web_search` tool, optionally behind basic auth.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearxngSection {
    url: String,
    username: Option<String>,
    password: Option<Secret>,
    #[serde(default = "SearxngSection::default_results")]
    results: usize,
}

impl SearxngSection {
    fn default_results() -> usize {
        5
    }

    fn validate(self) -> Result<Searxng> {
        let url = self.url.trim_end_matches('/').to_owned();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            bail!("tools.searxng.url must start with http:// or https://");
        }
        let auth = match (self.username, self.password) {
            (Some(user), Some(password)) => Some((user, password)),
            (None, None) => None,
            _ => bail!("tools.searxng needs both username and password for basic auth, or neither"),
        };
        if !(1..=20).contains(&self.results) {
            bail!("tools.searxng.results must be between 1 and 20");
        }
        Ok(Searxng {
            url,
            auth,
            results: self.results,
        })
    }
}

/// `[tools.brave]`: the Brave Search API for the `web_search` tool.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BraveSection {
    api_key: Secret,
    #[serde(default = "SearxngSection::default_results")]
    results: usize,
}

impl BraveSection {
    fn validate(self) -> Result<Brave> {
        if self.api_key.as_str().trim().is_empty() {
            bail!("tools.brave.api_key is empty");
        }
        if !(1..=20).contains(&self.results) {
            bail!("tools.brave.results must be between 1 and 20");
        }
        Ok(Brave {
            api_key: self.api_key,
            results: self.results,
        })
    }
}

/// Validated `[tools.brave]`.
#[derive(Debug, Clone)]
pub struct Brave {
    pub api_key: Secret,
    /// How many results `web_search` returns.
    pub results: usize,
}

/// Where `web_search` searches: one provider, picked in the config.
#[derive(Debug, Clone)]
pub enum Search {
    Searxng(Searxng),
    Brave(Brave),
}

impl Search {
    /// How many results `web_search` returns.
    pub fn results(&self) -> usize {
        match self {
            Self::Searxng(searxng) => searxng.results,
            Self::Brave(brave) => brave.results,
        }
    }
}

/// Validated `[tools.searxng]`.
#[derive(Debug, Clone)]
pub struct Searxng {
    /// Base URL without a trailing slash; `/search` is appended.
    pub url: String,
    /// Basic auth username and password.
    pub auth: Option<(String, Secret)>,
    /// How many results `web_search` returns.
    pub results: usize,
}

/// `[approval]`: who approves MCP calls and settings changes.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalSection {
    #[serde(default)]
    mode: ApprovalMode,
    /// Reviewer model for `auto`; defaults to `model.name`.
    model: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ApprovalMode {
    #[default]
    Ask,
    Auto,
}

/// How actions that need approval are approved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// Ask the user every time.
    Ask,
    /// A reviewer model passes low-risk actions; anything else is put to the user.
    Auto { model: String },
}

/// `[mcp.servers.<name>]`: MCP servers whose tools the model can call.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpSection {
    #[serde(default)]
    servers: BTreeMap<String, McpServer>,
}

/// One MCP server: a local program over stdio (`command`) or a remote one over HTTP (`url`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for `command`, often API tokens.
    #[serde(default)]
    pub env: BTreeMap<String, Secret>,
    pub url: Option<String>,
    /// Sent as `Authorization: Bearer <token>` to `url`.
    pub token: Option<Secret>,
    /// Ask the user before each call. Default true; turn off only for servers that can't change anything.
    #[serde(default = "McpServer::default_approve")]
    pub approve: bool,
}

impl McpServer {
    fn default_approve() -> bool {
        true
    }

    fn validate(&self, name: &str) -> Result<()> {
        // Tool names sent to the model are `<server>__<tool>`, limited to these characters.
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!("mcp.servers.{name}: names may use only letters, digits, _ and -");
        }
        match (&self.command, &self.url) {
            (Some(_), None) => {}
            (None, Some(url)) if url.starts_with("http://") || url.starts_with("https://") => {}
            (None, Some(_)) => bail!("mcp.servers.{name}.url must start with http:// or https://"),
            _ => bail!("mcp.servers.{name} needs exactly one of `command` or `url`"),
        }
        if self.command.is_some() && self.token.is_some() {
            bail!(
                "mcp.servers.{name}.token is for `url` servers; pass tokens to `command` in `env`"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogSection {
    #[serde(default = "LogSection::default_level")]
    level: String,
}

impl LogSection {
    fn default_level() -> String {
        "warn".to_owned()
    }
}

impl Default for LogSection {
    fn default() -> Self {
        Self {
            level: Self::default_level(),
        }
    }
}

/// `[discord]`: lets `mokuzu serve` chat through a Discord bot, by DM and in any server channel.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordSection {
    pub token: Secret,
    /// Discord user IDs allowed to talk to the bot in DMs and in servers not listed in `guilds`;
    /// everyone else is ignored.
    pub allowed_users: Vec<u64>,
    /// `[discord.guilds.<server ID>]`: rules for one server, used there instead of `allowed_users`.
    #[serde(default)]
    pub guilds: BTreeMap<u64, GuildRule>,
}

impl DiscordSection {
    fn guild(&self, guild: Option<u64>) -> Option<&GuildRule> {
        guild.and_then(|guild| self.guilds.get(&guild))
    }

    /// Whether `user` may talk to the bot in `guild` (`None` for DMs).
    pub fn can_talk(&self, guild: Option<u64>, user: u64) -> bool {
        match self.guild(guild).map(|rule| &rule.allowed_users) {
            Some(Users::Everyone) => true,
            Some(Users::Only(ids)) => ids.contains(&user),
            None => self.allowed_users.contains(&user),
        }
    }

    /// Whether `user` may approve actions in `guild`. In a server open to everyone, only the
    /// main `allowed_users` may.
    pub fn can_approve(&self, guild: Option<u64>, user: u64) -> bool {
        match self.guild(guild).map(|rule| &rule.allowed_users) {
            Some(Users::Only(ids)) => ids.contains(&user),
            Some(Users::Everyone) | None => self.allowed_users.contains(&user),
        }
    }

    /// Whether the bot answers only @mentions in `guild`.
    pub fn mention_only(&self, guild: Option<u64>) -> bool {
        self.guild(guild).is_some_and(|rule| rule.mention_only)
    }
}

/// One server's rules for the Discord bot.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuildRule {
    /// Who may talk to the bot in this server: user IDs, or `"*"` for everyone.
    pub allowed_users: Users,
    /// Answer only messages that @mention the bot, in its threads too.
    #[serde(default)]
    pub mention_only: bool,
}

/// A server's `allowed_users`: a non-empty list of user IDs, or `"*"` for everyone.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawUsers")]
pub enum Users {
    Everyone,
    Only(Vec<u64>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawUsers {
    Ids(Vec<u64>),
    Pattern(String),
}

impl TryFrom<RawUsers> for Users {
    type Error = String;

    fn try_from(raw: RawUsers) -> std::result::Result<Self, String> {
        match raw {
            RawUsers::Pattern(text) if text == "*" => Ok(Self::Everyone),
            RawUsers::Pattern(text) => Err(format!(
                "allowed_users is a list of user IDs or \"*\", not {text:?}"
            )),
            RawUsers::Ids(ids) if ids.is_empty() => {
                Err("allowed_users is empty; list user IDs or use \"*\"".to_owned())
            }
            RawUsers::Ids(ids) => Ok(Self::Only(ids)),
        }
    }
}

/// Validated settings with every default filled in.
#[derive(Debug, Clone)]
pub struct Config {
    pub model: String,
    pub max_tokens: u32,
    /// Estimated history size (tokens) at which older turns are summarized.
    pub compact_at_tokens: usize,
    pub api_key: Secret,
    pub api: Api,
    /// OpenCode Go base URL, e.g. `https://opencode.ai/zen/go/v1`.
    pub base_url: String,
    /// Enables the `web_search` tool when set.
    pub search: Option<Search>,
    /// MCP servers by name.
    pub mcp: BTreeMap<String, McpServer>,
    pub approval: Approval,
    /// `tracing` filter directive, e.g. `warn` or `mokuzu=debug`.
    pub log_level: String,
    pub discord: Option<DiscordSection>,
    /// SQLite file holding conversation history.
    pub database_path: PathBuf,
    /// The user's time zone, for message timestamps and scheduled jobs.
    pub timezone: chrono_tz::Tz,
    /// File this was loaded from; empty when parsed from text.
    pub path: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            bail!("no config at {}; run `mokuzu configure`", path.display());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config =
            Self::parse(&text).with_context(|| format!("invalid config {}", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            ..config
        })
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: File = toml::from_str(text)?;
        if file
            .discord
            .as_ref()
            .is_some_and(|d| d.allowed_users.is_empty())
        {
            bail!("discord.allowed_users is empty; list your Discord user ID");
        }
        let database_path = match file.database.path {
            Some(path) => path,
            None => std::env::home_dir()
                .context("cannot find home directory for the default database path")?
                .join(".local/share/mokuzu/mokuzu.db"),
        };
        let base_url = file
            .opencode_go
            .base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());

        let compact_at_tokens = file
            .model
            .compact_at_tokens
            .unwrap_or(DEFAULT_COMPACT_AT_TOKENS);
        if compact_at_tokens < MIN_COMPACT_AT_TOKENS {
            bail!("model.compact_at_tokens must be at least {MIN_COMPACT_AT_TOKENS}");
        }
        for (name, server) in &file.mcp.servers {
            server.validate(name)?;
        }
        let timezone = match file.timezone {
            Some(name) => name.parse().map_err(|_| {
                anyhow::anyhow!("timezone {name:?} is not an IANA time zone like Asia/Taipei")
            })?,
            None => system_timezone(),
        };
        let model = file.model.name.unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        let approval = match file.approval.mode {
            ApprovalMode::Ask => Approval::Ask,
            ApprovalMode::Auto => Approval::Auto {
                model: file.approval.model.unwrap_or_else(|| model.clone()),
            },
        };
        Ok(Self {
            approval,
            api: file.model.api.unwrap_or_else(|| Api::for_model(&model)),
            model,
            max_tokens: file.model.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            compact_at_tokens,
            api_key: file.opencode_go.api_key,
            base_url: base_url.trim_end_matches('/').to_owned(),
            search: match (file.tools.searxng, file.tools.brave) {
                (Some(_), Some(_)) => {
                    bail!("set only one of [tools.searxng] and [tools.brave] for web search")
                }
                (Some(searxng), None) => Some(Search::Searxng(searxng.validate()?)),
                (None, Some(brave)) => Some(Search::Brave(brave.validate()?)),
                (None, None) => None,
            },
            mcp: file.mcp.servers,
            log_level: file.log.level,
            discord: file.discord,
            database_path,
            timezone,
            path: PathBuf::new(),
        })
    }
}

/// This machine's time zone (`TZ`, else the system setting), or UTC if it can't be read.
pub fn system_timezone() -> chrono_tz::Tz {
    match iana_time_zone::get_timezone().map(|name| name.parse()) {
        Ok(Ok(zone)) => zone,
        _ => {
            tracing::warn!("cannot read this machine's time zone; using UTC");
            chrono_tz::Tz::UTC
        }
    }
}

/// Writes the config readable only by the user, keeping the previous file as `.bak`.
pub fn write(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    if path.exists() {
        let backup = path.with_extension("toml.bak");
        std::fs::copy(path, &backup)
            .with_context(|| format!("failed to back up to {}", backup.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    // `mode` only applies on creation; tighten an existing file too.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let config = Config::parse(include_str!("../../../config.example.toml")).expect("valid");
        assert_eq!(config.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(config.api, Api::Anthropic);
    }

    #[test]
    fn timezone_parses_and_bad_ones_are_refused() {
        let base = "[opencode-go]\napi_key = \"k\"\n";
        let config = Config::parse(&format!("timezone = \"Asia/Taipei\"\n{base}")).expect("valid");
        assert_eq!(config.timezone, chrono_tz::Tz::Asia__Taipei);
        assert!(Config::parse(&format!("timezone = \"Taipei\"\n{base}")).is_err());
    }

    #[test]
    fn mcp_servers_parse_and_bad_ones_are_refused() {
        let base = "[opencode-go]\napi_key = \"k\"\n";
        let config = Config::parse(&format!(
            "{base}[mcp.servers.fs]\ncommand = \"npx\"\nargs = [\"-y\", \"x\"]\n\
             [mcp.servers.remote]\nurl = \"https://e.com/mcp\"\ntoken = \"t\"\napprove = false\n"
        ))
        .expect("valid");
        assert!(config.mcp["fs"].approve);
        assert!(!config.mcp["remote"].approve);
        for bad in [
            "[mcp.servers.fs]\n",
            "[mcp.servers.fs]\ncommand = \"x\"\nurl = \"https://e.com\"\n",
            "[mcp.servers.fs]\nurl = \"ftp://e.com\"\n",
            "[mcp.servers.\"a b\"]\ncommand = \"x\"\n",
        ] {
            assert!(Config::parse(&format!("{base}{bad}")).is_err(), "{bad}");
        }
    }

    #[test]
    fn approval_defaults_to_ask_and_auto_reviewer_defaults_to_main_model() {
        let base = "[opencode-go]\napi_key = \"k\"\n[model]\nname = \"glm-5.3\"\n";
        assert_eq!(Config::parse(base).expect("valid").approval, Approval::Ask);
        let auto = Config::parse(&format!("{base}[approval]\nmode = \"auto\"\n")).expect("valid");
        assert_eq!(
            auto.approval,
            Approval::Auto {
                model: "glm-5.3".to_owned()
            }
        );
        let picked = format!("{base}[approval]\nmode = \"auto\"\nmodel = \"kimi-k3\"\n");
        assert_eq!(
            Config::parse(&picked).expect("valid").approval,
            Approval::Auto {
                model: "kimi-k3".to_owned()
            }
        );
    }

    #[test]
    fn old_tool_sections_still_load() {
        let text = "[opencode-go]\napi_key = \"k\"\n\n[tools.bash]\ntimeout_secs = 60\n\n\
                    [tools.claude_code]\ndirs = [\"~/repos\"]\n";
        assert!(Config::parse(text).is_ok());
    }

    #[test]
    fn minimal_config_fills_defaults_and_redacts_key() {
        let config = Config::parse(
            r#"
            [opencode-go]
            api_key = "sk-secret"
            base_url = "http://localhost:8080/v1/"
            "#,
        )
        .expect("valid");
        assert_eq!(config.model, "minimax-m3");
        assert_eq!(config.max_tokens, 16_000);
        assert_eq!(config.compact_at_tokens, 200_000);
        assert_eq!(config.base_url, "http://localhost:8080/v1");
        assert_eq!(Api::for_model("glm-5.3"), Api::Openai);
        assert_eq!(Api::for_model("claude-haiku-5-5"), Api::Anthropic);
        assert_eq!(Api::for_model("gpt-6-luna"), Api::Responses);
        assert_eq!(Api::for_model("gemini-4-argon-contributor"), Api::Gemini);
        assert_eq!(config.log_level, "warn");
        assert!(config.discord.is_none());
        assert!(
            config
                .database_path
                .ends_with(".local/share/mokuzu/mokuzu.db")
        );
        assert!(!format!("{config:?}").contains("sk-secret"));
    }

    #[test]
    fn searxng_section_validates_auth_pairs() {
        let base = "[opencode-go]\napi_key = \"k\"\n[tools.searxng]\n";
        let config = Config::parse(&format!(
            "{base}url = \"https://search.example.com/\"\nusername = \"me\"\npassword = \"pw\""
        ))
        .expect("valid");
        let Some(Search::Searxng(searxng)) = config.search else {
            panic!("searxng configured");
        };
        assert_eq!(searxng.url, "https://search.example.com");
        assert_eq!(searxng.results, 5);
        assert_eq!(searxng.auth.map(|(user, _)| user).as_deref(), Some("me"));
        assert!(Config::parse(&format!("{base}url = \"http://x\"\npassword = \"pw\"")).is_err());
        assert!(
            !format!(
                "{:?}",
                Config::parse(&format!(
                    "{base}url = \"http://x\"\nusername = \"me\"\npassword = \"pw\""
                ))
                .expect("valid")
            )
            .contains("\"pw\"")
        );
        assert!(Config::parse(&format!("{base}url = \"http://x\"\nusername = \"me\"")).is_err());
        assert!(Config::parse(&format!("{base}url = \"x\"")).is_err());
        assert!(Config::parse(&format!("{base}url = \"http://x\"\nresults = 0")).is_err());
    }

    #[test]
    fn brave_section_parses_and_excludes_searxng() {
        let base = "[opencode-go]\napi_key = \"k\"\n";
        let config = Config::parse(&format!(
            "{base}[tools.brave]\napi_key = \"bk\"\nresults = 8\n"
        ))
        .expect("valid");
        let Some(Search::Brave(brave)) = &config.search else {
            panic!("brave configured");
        };
        assert_eq!((brave.api_key.as_str(), brave.results), ("bk", 8));
        assert!(!format!("{config:?}").contains("\"bk\""));
        assert!(Config::parse(&format!("{base}[tools.brave]\napi_key = \" \"\n")).is_err());
        assert!(
            Config::parse(&format!(
                "{base}[tools.brave]\napi_key = \"bk\"\n[tools.searxng]\nurl = \"http://x\"\n"
            ))
            .is_err(),
            "only one search provider"
        );
    }

    #[test]
    fn rejects_missing_key_unknown_fields_and_open_bot() {
        assert!(Config::parse("[model]\nname = \"x\"").is_err());
        let typo = "[model]\nnmae = \"x\"\n[opencode-go]\napi_key = \"k\"";
        assert!(Config::parse(typo).is_err());
        let old_format = "[model]\nprovider = \"anthropic\"\n[opencode-go]\napi_key = \"k\"";
        assert!(Config::parse(old_format).is_err());
        let open_bot =
            "[opencode-go]\napi_key = \"k\"\n[discord]\ntoken = \"t\"\nallowed_users = []";
        assert!(Config::parse(open_bot).is_err());
    }

    #[test]
    fn discord_guilds_override_allowed_users() {
        let base =
            "[opencode-go]\napi_key = \"k\"\n[discord]\ntoken = \"t\"\nallowed_users = [1]\n";
        let config = Config::parse(&format!(
            "{base}[discord.guilds.100]\nallowed_users = [2, 3]\nmention_only = true\n\
             [discord.guilds.200]\nallowed_users = [1]\n"
        ))
        .expect("valid");
        let discord = config.discord.expect("discord configured");
        assert!(discord.can_talk(None, 1) && !discord.can_talk(None, 2));
        assert!(discord.can_talk(Some(100), 2) && !discord.can_talk(Some(100), 1));
        assert!(discord.can_approve(Some(100), 2) && !discord.can_approve(Some(100), 1));
        assert!(discord.mention_only(Some(100)) && !discord.mention_only(Some(200)));
        assert!(discord.can_talk(Some(300), 1) && !discord.mention_only(None));

        let open = Config::parse(&format!(
            "{base}[discord.guilds.100]\nallowed_users = \"*\"\n"
        ))
        .expect("valid")
        .discord
        .expect("discord configured");
        assert!(open.can_talk(Some(100), 99), "everyone may talk");
        assert!(
            !open.can_approve(Some(100), 99),
            "only the main list approves"
        );
        assert!(open.can_approve(Some(100), 1));
        assert!(!open.can_talk(None, 99), "DMs keep the main list");
        assert!(
            Config::parse(&format!(
                "{base}[discord.guilds.100]\nallowed_users = \"all\"\n"
            ))
            .is_err()
        );
        assert!(
            Config::parse(&format!("{base}[discord.guilds.100]\nallowed_users = []\n")).is_err()
        );
        assert!(
            Config::parse(&format!("{base}[discord.guilds.x]\nallowed_users = [1]\n")).is_err()
        );
        assert!(
            Config::parse(&format!(
                "{base}[discord.guilds.100]\nallowed_users = [1]\nmentoin = true\n"
            ))
            .is_err()
        );
    }
}

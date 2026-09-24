//! TOML config file: OpenCode Go model and key, tools, logging, storage, and Discord.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
const DEFAULT_MODEL: &str = "minimax-m3";
const DEFAULT_MAX_TOKENS: u32 = 16_000;
const DEFAULT_COMPACT_AT_TOKENS: usize = 100_000;

/// Which OpenCode Go endpoint format a model speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Api {
    /// `/messages`: MiniMax and Qwen.
    Anthropic,
    /// `/chat/completions`: GLM, Kimi, DeepSeek, and the rest.
    Openai,
}

impl Api {
    /// OpenCode Go serves MiniMax and Qwen in Anthropic format and most others as chat completions.
    // ponytail: name-prefix guess; set `model.api` for anything it gets wrong.
    pub fn for_model(model: &str) -> Self {
        if model.starts_with("minimax") || model.starts_with("qwen") {
            Self::Anthropic
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
    #[serde(default)]
    model: ModelSection,
    #[serde(rename = "opencode-go")]
    opencode_go: OpenCodeGoSection,
    #[serde(default)]
    tools: ToolsSection,
    #[serde(default)]
    log: LogSection,
    discord: Option<DiscordSection>,
    #[serde(default)]
    database: DatabaseSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseSection {
    /// Defaults to `~/.local/share/mitten/mitten.db`.
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

/// OpenCode Go serves MiniMax and Qwen models through an Anthropic-format Messages endpoint.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenCodeGoSection {
    api_key: Secret,
    base_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsSection {
    #[serde(default)]
    bash: BashSection,
    searxng: Option<SearxngSection>,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BashSection {
    #[serde(default = "BashSection::default_timeout_secs")]
    timeout_secs: u64,
}

impl BashSection {
    fn default_timeout_secs() -> u64 {
        120
    }
}

impl Default for BashSection {
    fn default() -> Self {
        Self {
            timeout_secs: Self::default_timeout_secs(),
        }
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

/// `[discord]`: lets `mitten serve` chat through a Discord bot, by DM and in any server channel.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordSection {
    pub token: Secret,
    /// Discord user IDs allowed to talk to the bot; everyone else is ignored.
    pub allowed_users: Vec<u64>,
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
    pub bash_timeout: Duration,
    /// Enables the `web_search` tool when set.
    pub searxng: Option<Searxng>,
    /// `tracing` filter directive, e.g. `warn` or `mitten=debug`.
    pub log_level: String,
    pub discord: Option<DiscordSection>,
    /// SQLite file holding conversation history.
    pub database_path: PathBuf,
    /// File this was loaded from; empty when parsed from text.
    pub path: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            bail!("no config at {}; run `mitten configure`", path.display());
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
                .join(".local/share/mitten/mitten.db"),
        };
        let base_url = file
            .opencode_go
            .base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());

        let compact_at_tokens = file
            .model
            .compact_at_tokens
            .unwrap_or(DEFAULT_COMPACT_AT_TOKENS);
        if compact_at_tokens < 10_000 {
            bail!("model.compact_at_tokens must be at least 10000");
        }
        let model = file.model.name.unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        Ok(Self {
            api: file.model.api.unwrap_or_else(|| Api::for_model(&model)),
            model,
            max_tokens: file.model.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            compact_at_tokens,
            api_key: file.opencode_go.api_key,
            base_url: base_url.trim_end_matches('/').to_owned(),
            bash_timeout: Duration::from_secs(file.tools.bash.timeout_secs),
            searxng: file
                .tools
                .searxng
                .map(SearxngSection::validate)
                .transpose()?,
            log_level: file.log.level,
            discord: file.discord,
            database_path,
            path: PathBuf::new(),
        })
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
        assert_eq!(config.compact_at_tokens, 100_000);
        assert_eq!(config.base_url, "http://localhost:8080/v1");
        assert_eq!(Api::for_model("glm-5.3"), Api::Openai);
        assert_eq!(config.bash_timeout, Duration::from_secs(120));
        assert_eq!(config.log_level, "warn");
        assert!(config.discord.is_none());
        assert!(
            config
                .database_path
                .ends_with(".local/share/mitten/mitten.db")
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
        let searxng = config.searxng.expect("configured");
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
}

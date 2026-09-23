//! TOML config file: OpenCode Go model and key, tools, logging, storage, and Discord.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
const DEFAULT_MODEL: &str = "minimax-m3";
const DEFAULT_MAX_TOKENS: u32 = 16_000;

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
    max_tokens: Option<u32>,
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

/// `[discord]`: lets `mitten serve` take DMs from a Discord bot.
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
    pub api_key: Secret,
    /// Full Messages endpoint, e.g. `https://opencode.ai/zen/go/v1/messages`.
    pub messages_url: String,
    pub bash_timeout: Duration,
    /// `tracing` filter directive, e.g. `warn` or `mitten=debug`.
    pub log_level: String,
    pub discord: Option<DiscordSection>,
    /// SQLite file holding conversation history.
    pub database_path: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            bail!("no config at {}; run `mitten configure`", path.display());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid config {}", path.display()))
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

        Ok(Self {
            model: file.model.name.unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
            max_tokens: file.model.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            api_key: file.opencode_go.api_key,
            messages_url: format!("{}/messages", base_url.trim_end_matches('/')),
            bash_timeout: Duration::from_secs(file.tools.bash.timeout_secs),
            log_level: file.log.level,
            discord: file.discord,
            database_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let config = Config::parse(include_str!("../../../config.example.toml")).expect("valid");
        assert_eq!(
            config.messages_url,
            "https://opencode.ai/zen/go/v1/messages"
        );
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
        assert_eq!(config.messages_url, "http://localhost:8080/v1/messages");
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

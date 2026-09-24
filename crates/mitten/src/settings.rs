//! `settings` tool: reads and changes a few non-secret config values, like which model to use.
//! Keys, tokens, Discord access, and paths stay out of reach; edits keep the file's comments.

use anyhow::{Context, Result, bail};
use rig_core::completion::ToolDefinition;
use serde_json::{Value, json};
use toml_edit::{DocumentMut, TableLike};

use crate::config::Config;
use crate::onboarding::MODELS;

/// The only keys the tool may change, as `section.key`. All but `model.name` hold positive integers.
const KEYS: &[&str] = &[
    "model.name",
    "model.max_tokens",
    "model.compact_at_tokens",
    "tools.bash.timeout_secs",
];

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "settings".to_owned(),
        description: format!(
            "Read or change Mitten's own settings in its config file. `get` lists the current \
             values and known models. `set` changes one key after the user approves; a new model \
             is tested before it is saved. Changes apply to every conversation from the next \
             request. Editable keys: {}.",
            KEYS.join(", ")
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["get", "set"]},
                "key": {"type": "string", "enum": KEYS},
                "value": {
                    "type": ["string", "integer"],
                    "description": "For set: the new value, a model name or a positive integer.",
                },
            },
            "required": ["action"],
        }),
    }
}

/// Current editable values plus the model presets, for `get`.
pub fn show(config: &Config) -> String {
    format!(
        "model.name = {}\nmodel.max_tokens = {}\nmodel.compact_at_tokens = {}\n\
         tools.bash.timeout_secs = {}\nknown models: {}",
        config.model,
        config.max_tokens,
        config.compact_at_tokens,
        config.bash_timeout.as_secs(),
        MODELS.join(", "),
    )
}

/// Sets `key` to `value` in the config `text`; returns the new text and the config it parses to.
/// Changing the model also drops `model.api`, so the endpoint format is guessed for the new name.
pub fn edit(text: &str, key: &str, value: &Value) -> Result<(String, Config)> {
    if !KEYS.contains(&key) {
        bail!(
            "`{key}` is not editable; editable keys: {}",
            KEYS.join(", ")
        );
    }
    let item = if key == "model.name" {
        let name = value
            .as_str()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .context("model.name needs a model name")?;
        toml_edit::value(name)
    } else {
        let number = value
            .as_i64()
            .or_else(|| value.as_str()?.trim().parse().ok())
            .filter(|number| *number > 0)
            .with_context(|| format!("{key} needs a positive integer"))?;
        toml_edit::value(number)
    };
    let mut doc: DocumentMut = text.parse().context("config is not valid TOML")?;
    let (sections, field) = key.rsplit_once('.').context("key has no section")?;
    let mut table: &mut dyn TableLike = doc.as_table_mut();
    for section in sections.split('.') {
        table = table
            .entry(section)
            .or_insert_with(toml_edit::table)
            .as_table_like_mut()
            .with_context(|| format!("`{section}` in the config is not a table"))?;
    }
    table.insert(field, item);
    if key == "model.name" {
        table.remove("api");
    }
    let text = doc.to_string();
    let config = Config::parse(&text)?;
    Ok((text, config))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "# keep me\n[model]\nname = \"minimax-m3\"\napi = \"anthropic\"\n\n\
                        [opencode-go]\napi_key = \"k\"\n";

    #[test]
    fn edit_changes_one_key_and_keeps_comments() {
        let (text, config) = edit(BASE, "model.name", &json!("glm-5.3")).expect("valid");
        assert!(text.starts_with("# keep me\n"));
        assert!(!text.contains("api = "));
        assert_eq!(config.model, "glm-5.3");
        assert_eq!(config.api, crate::config::Api::Openai);

        let (text, config) = edit(BASE, "tools.bash.timeout_secs", &json!("30")).expect("valid");
        assert!(text.contains("timeout_secs = 30"));
        assert_eq!(config.bash_timeout.as_secs(), 30);
        assert_eq!(config.model, "minimax-m3");
    }

    #[test]
    fn edit_rejects_secrets_bad_values_and_invalid_results() {
        assert!(edit(BASE, "opencode-go.api_key", &json!("x")).is_err());
        assert!(edit(BASE, "discord.allowed_users", &json!(1)).is_err());
        assert!(edit(BASE, "model.max_tokens", &json!(0)).is_err());
        assert!(edit(BASE, "model.name", &json!(" ")).is_err());
        assert!(edit(BASE, "model.compact_at_tokens", &json!(500)).is_err());
    }
}

//! Read-only file tools: `read_file` and `list_dir`. They run without approval, so they refuse
//! hidden paths (any component starting with `.`, like `~/.ssh` or `.env`) and Mitten's own config
//! and database, which hold API keys, tokens, and every conversation.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use rig_core::completion::ToolDefinition;
use serde_json::{Value, json};

use crate::config::Config;

/// Characters returned per `read_file` call; `offset` pages through the rest.
const MAX_CHARS: usize = 15_000;
/// Largest file `read_file` opens.
const MAX_BYTES: u64 = 5_000_000;
/// Entries listed per `list_dir` call.
const MAX_ENTRIES: usize = 500;

pub fn read_tool() -> ToolDefinition {
    ToolDefinition {
        name: "read_file".to_owned(),
        description: format!(
            "Read a text file on the user's machine. Returns up to {MAX_CHARS} characters; for longer \
             files, call again with the `offset` it gives you. Paths may be absolute, start with ~/, \
             or be relative to the working directory. Hidden files and directories are refused."
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file to read."},
                "offset": {
                    "type": "integer",
                    "description": "Character offset to continue from, for long files. Default 0.",
                },
            },
            "required": ["path"],
        }),
    }
}

pub fn list_tool() -> ToolDefinition {
    ToolDefinition {
        name: "list_dir".to_owned(),
        description: format!(
            "List a directory on the user's machine: subdirectories end with /, files show their \
             size. Up to {MAX_ENTRIES} entries; hidden entries are left out."
        ),
        parameters: json!({
            "type": "object",
            "properties": {"path": {"type": "string", "description": "The directory to list."}},
            "required": ["path"],
        }),
    }
}

/// Runs one `read_file` call; failures come back as text for the model.
pub async fn read(config: &Config, args: &Value) -> String {
    let offset = args["offset"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);
    match read_text(config, &args["path"]).await {
        Ok((path, text)) => page(&path, &text, offset),
        Err(err) => format!("error: {err:#}"),
    }
}

/// Runs one `list_dir` call; failures come back as text for the model.
pub async fn list(config: &Config, args: &Value) -> String {
    match list_entries(config, &args["path"]).await {
        Ok(text) => text,
        Err(err) => format!("error: {err:#}"),
    }
}

async fn read_text(config: &Config, path: &Value) -> Result<(PathBuf, String)> {
    let path = resolve(config, path).await?;
    let size = tokio::fs::metadata(&path).await?.len();
    if size > MAX_BYTES {
        bail!(
            "{} is {size} bytes, over the {MAX_BYTES}-byte limit",
            path.display()
        );
    }
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("cannot read {}", path.display()))?;
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not a text file", path.display()))?;
    Ok((path, text))
}

async fn list_entries(config: &Config, path: &Value) -> Result<String> {
    let path = resolve(config, path).await?;
    let mut dir = tokio::fs::read_dir(&path)
        .await
        .with_context(|| format!("cannot list {}", path.display()))?;
    let mut entries = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        entries.push(match entry.metadata().await {
            Ok(meta) if meta.is_dir() => format!("{name}/"),
            Ok(meta) => format!("{name}  ({} bytes)", meta.len()),
            Err(_) => name,
        });
    }
    entries.sort();
    let total = entries.len();
    entries.truncate(MAX_ENTRIES);
    let mut out = format!(
        "{}: {total} entries\n{}",
        path.display(),
        entries.join("\n")
    );
    if total > MAX_ENTRIES {
        out.push_str(&format!("\n[{} more not shown]", total - MAX_ENTRIES));
    }
    Ok(out)
}

/// The canonical form of the `path` argument, if the tools may open it.
async fn resolve(config: &Config, path: &Value) -> Result<PathBuf> {
    let Some(path) = path.as_str().map(str::trim).filter(|p| !p.is_empty()) else {
        bail!("`path` is required");
    };
    let path = match path.strip_prefix("~/").or((path == "~").then_some("")) {
        Some(rest) => std::env::home_dir()
            .context("cannot find home directory")?
            .join(rest),
        None => PathBuf::from(path),
    };
    // Canonical, so `..` and symlinks can't reach around the checks below.
    let path = tokio::fs::canonicalize(&path)
        .await
        .with_context(|| format!("cannot open {}", path.display()))?;
    if is_hidden(&path) {
        bail!("{} is hidden; hidden paths are refused", path.display());
    }
    for secret in [&config.path, &config.database_path] {
        if let Ok(secret) = tokio::fs::canonicalize(secret).await
            && secret == path
        {
            bail!("{} is Mitten's own config or database", path.display());
        }
    }
    Ok(path)
}

fn is_hidden(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(name) => name.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

fn page(path: &Path, text: &str, offset: usize) -> String {
    let total = text.chars().count();
    if total > 0 && offset >= total {
        return format!("error: offset {offset} is past the end ({total} characters)");
    }
    let chunk: String = text.chars().skip(offset).take(MAX_CHARS).collect();
    let end = offset + chunk.chars().count();
    let mut out = format!(
        "File: {}\nCharacters {offset}-{end} of {total}\n\n{chunk}",
        path.display()
    );
    if end < total {
        out.push_str(&format!(
            "\n\n[{} more characters; call read_file again with offset={end}]",
            total - end
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_components_are_detected() {
        assert!(is_hidden(Path::new("/home/me/.ssh/id_ed25519")));
        assert!(is_hidden(Path::new("/srv/app/.env")));
        assert!(!is_hidden(Path::new("/home/me/notes/todo.md")));
    }

    #[tokio::test]
    async fn tools_refuse_hidden_and_own_files_and_page_long_ones() {
        let dir = std::env::temp_dir().join(format!("mitten-files-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).expect("create dirs");
        std::fs::write(dir.join("long.txt"), "é".repeat(MAX_CHARS + 5)).expect("write");
        std::fs::write(dir.join("config.toml"), "secret").expect("write");
        std::fs::write(dir.join("sub/.env"), "secret").expect("write");
        let config = Config {
            path: dir.join("config.toml"),
            ..Config::parse("[opencode-go]\napi_key = \"k\"\n").expect("valid")
        };
        let path = |p: &str| json!({ "path": dir.join(p) });

        let first = read(&config, &path("long.txt")).await;
        assert!(first.contains(&format!("offset={MAX_CHARS}")), "{first}");
        let args = json!({ "path": dir.join("long.txt"), "offset": MAX_CHARS });
        assert!(read(&config, &args).await.ends_with("ééééé"));
        assert!(
            read(&config, &path("config.toml"))
                .await
                .starts_with("error:")
        );
        assert!(read(&config, &path("sub/.env")).await.starts_with("error:"));
        assert!(
            read(&config, &path("sub/../sub/.env"))
                .await
                .starts_with("error:")
        );

        let listing = list(&config, &path("")).await;
        assert!(
            listing.contains("sub/") && listing.contains("long.txt"),
            "{listing}"
        );
        assert!(!list(&config, &path("sub")).await.contains(".env"));
        std::fs::remove_dir_all(&dir).expect("clean up");
    }
}

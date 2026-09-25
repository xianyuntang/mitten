//! `claude_code` tool: hands a coding task to Claude Code (`claude -p`) in an allowed directory,
//! shows its steps as status notes, and returns its final report and session id so later calls can
//! continue the same session.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use rig_core::completion::ToolDefinition;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

use crate::agent::Io;
use crate::config::ClaudeCode;

/// Report characters returned to the model.
const MAX_REPORT_CHARS: usize = 20_000;
/// Longest step detail shown in a status note.
const MAX_STEP_CHARS: usize = 80;

pub fn tool(config: &ClaudeCode) -> ToolDefinition {
    ToolDefinition {
        name: "claude_code".to_owned(),
        description: format!(
            "Hand a coding task to Claude Code, an autonomous coding agent that can read, edit, and \
             run code in a directory, and get its final report back. Use it for work you can't do \
             with your read-only tools: changing code, running builds or tests, git. Write `task` as \
             a complete brief (goal, constraints, what done looks like); Claude Code does not see \
             this conversation. Allowed directories (and anything under them): {}. Each result \
             includes a session id; pass it as `session` to continue that work with its context.",
            dirs_list(config)
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "task": {"type": "string", "description": "The full task brief."},
                "dir": {"type": "string", "description": "Directory to work in; ~/ is allowed."},
                "session": {
                    "type": "string",
                    "description": "Session id from an earlier claude_code result, to continue it.",
                },
            },
            "required": ["task", "dir"],
        }),
    }
}

/// System prompt addition when the tool is on.
pub fn guidance(config: &ClaudeCode) -> String {
    format!(
        "# Claude Code\nFor work that changes code or needs commands run inside {}, delegate to the \
         claude_code tool instead of telling the user what to run. Brief it fully, then report its \
         result to the user in your own words, including anything it could not finish.",
        dirs_list(config)
    )
}

fn dirs_list(config: &ClaudeCode) -> String {
    config
        .dirs
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Runs one tool call after asking the user; failures come back as text for the model.
pub async fn run(config: &ClaudeCode, args: &Value, io: &mut impl Io) -> Result<String> {
    let Some(task) = args["task"]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Ok("error: `task` is required".to_owned());
    };
    let session = args["session"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let dir = match resolve(config, &args["dir"]).await {
        Ok(dir) => dir,
        Err(err) => return Ok(format!("error: {err:#}")),
    };
    let resumed = session.map_or(String::new(), |id| format!(" (resuming {id})"));
    if !io
        .confirm(&format!(
            "claude code in {}{resumed}\n{task}",
            dir.display()
        ))
        .await?
    {
        return Ok("error: the user denied this task".to_owned());
    }
    let run = tokio::time::timeout(config.timeout, execute(config, &dir, task, session, io)).await;
    let (report, ok) = match run {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(err)) => (format!("{err:#}"), false),
        Err(_) => (
            format!(
                "Claude Code did not finish within {:?} and was stopped",
                config.timeout
            ),
            false,
        ),
    };
    io.ran(&report, ok).await?;
    Ok(if ok {
        report
    } else {
        format!("error: {report}")
    })
}

/// The canonical `dir`, if it is one of the allowed directories or inside one.
async fn resolve(config: &ClaudeCode, dir: &Value) -> Result<PathBuf> {
    let Some(dir) = dir.as_str().map(str::trim).filter(|d| !d.is_empty()) else {
        bail!("`dir` is required");
    };
    let dir = tokio::fs::canonicalize(expand(Path::new(dir))?)
        .await
        .with_context(|| format!("cannot open {dir}"))?;
    for allowed in &config.dirs {
        if let Ok(allowed) = tokio::fs::canonicalize(expand(allowed)?).await
            && dir.starts_with(&allowed)
        {
            return Ok(dir);
        }
    }
    bail!(
        "{} is outside the allowed directories: {}",
        dir.display(),
        dirs_list(config)
    )
}

fn expand(path: &Path) -> Result<PathBuf> {
    Ok(match path.strip_prefix("~") {
        Ok(rest) => std::env::home_dir()
            .context("cannot find home directory")?
            .join(rest),
        Err(_) => path.to_owned(),
    })
}

/// Runs Claude Code to completion; returns its report and whether it succeeded.
async fn execute(
    config: &ClaudeCode,
    dir: &Path,
    task: &str,
    session: Option<&str>,
    io: &mut impl Io,
) -> Result<(String, bool)> {
    let mut command = tokio::process::Command::new(&config.command);
    command
        .args(["-p", "--output-format", "stream-json", "--verbose"])
        .args(["--permission-mode", &config.permission_mode])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(id) = session {
        command.args(["--resume", id]);
    }
    if !config.allowed_tools.is_empty() {
        command.arg("--allowedTools").args(&config.allowed_tools);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run {}", config.command))?;
    // The task goes in on stdin, so one starting with `-` can't be read as a flag.
    let mut stdin = child.stdin.take().context("no stdin")?;
    stdin.write_all(task.as_bytes()).await?;
    drop(stdin);
    let mut stderr = child.stderr.take().context("no stderr")?;
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });

    let mut lines = tokio::io::BufReader::new(child.stdout.take().context("no stdout")?).lines();
    let mut outcome = None;
    while let Some(line) = lines.next_line().await? {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("assistant") => {
                for step in steps(&event) {
                    io.note(&format!("🤖 {step}")).await?;
                }
            }
            Some("result") => outcome = Some(report(&event)),
            _ => {}
        }
    }
    let status = child.wait().await?;
    match outcome {
        Some(outcome) => Ok(outcome),
        None => {
            let errors = errors.await.unwrap_or_default();
            bail!(
                "Claude Code exited ({status}) without a result: {}",
                errors.trim()
            )
        }
    }
}

/// One line per tool Claude Code used in an `assistant` event, e.g. `Edit src/main.rs`.
fn steps(event: &Value) -> Vec<String> {
    let Some(content) = event["message"]["content"].as_array() else {
        return Vec::new();
    };
    content
        .iter()
        .filter(|c| c["type"] == "tool_use")
        .map(|c| {
            let name = c["name"].as_str().unwrap_or("tool");
            let input = &c["input"];
            let detail = [
                "file_path",
                "command",
                "pattern",
                "path",
                "url",
                "description",
            ]
            .iter()
            .find_map(|key| input[*key].as_str())
            .unwrap_or_default();
            let line = detail.lines().next().unwrap_or_default();
            let mut detail: String = line.chars().take(MAX_STEP_CHARS).collect();
            if detail.len() < line.len() {
                detail.push('…');
            }
            format!("{name} {detail}").trim_end().to_owned()
        })
        .collect()
}

/// The model-facing report from the final `result` event.
fn report(event: &Value) -> (String, bool) {
    let ok = event["is_error"] != true && event["subtype"] == "success";
    let result = event["result"].as_str().unwrap_or_default();
    let mut text: String = result.chars().take(MAX_REPORT_CHARS).collect();
    if text.len() < result.len() {
        text.push_str("\n[report truncated]");
    }
    let session = event["session_id"].as_str().unwrap_or("unknown");
    let turns = event["num_turns"].as_u64().unwrap_or_default();
    let cost = event["total_cost_usd"].as_f64().unwrap_or_default();
    (
        format!("session: {session}\nturns: {turns}, cost: ${cost:.2}\n\n{text}"),
        ok,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_and_report_read_stream_json() {
        let event = json!({"type": "assistant", "message": {"content": [
            {"type": "thinking", "thinking": ""},
            {"type": "tool_use", "name": "Edit", "input": {"file_path": "src/main.rs"}},
            {"type": "tool_use", "name": "Bash", "input": {"command": "cargo test\n--more"}},
            {"type": "tool_use", "name": "TodoWrite", "input": {"todos": []}},
        ]}});
        assert_eq!(
            steps(&event),
            ["Edit src/main.rs", "Bash cargo test", "TodoWrite"]
        );
        let (text, ok) = report(&json!({
            "type": "result", "subtype": "success", "is_error": false, "result": "Done.",
            "session_id": "abc", "num_turns": 3, "total_cost_usd": 0.1234,
        }));
        assert!(ok);
        assert_eq!(text, "session: abc\nturns: 3, cost: $0.12\n\nDone.");
        let (_, ok) =
            report(&json!({"type": "result", "subtype": "error_max_turns", "is_error": true}));
        assert!(!ok);
    }

    /// Approves everything and records notes.
    #[derive(Default)]
    struct Recorder {
        notes: Vec<String>,
        asked: usize,
    }

    impl Io for Recorder {
        async fn say(&mut self, _text: &str) -> Result<()> {
            Ok(())
        }
        async fn note(&mut self, text: &str) -> Result<()> {
            self.notes.push(text.to_owned());
            Ok(())
        }
        async fn confirm(&mut self, _action: &str) -> Result<bool> {
            self.asked += 1;
            Ok(true)
        }
    }

    /// Runs the real `claude`: `cargo test -- --ignored claude_code_writes`.
    #[tokio::test]
    #[ignore = "runs Claude Code, which uses the account it is logged in with"]
    async fn claude_code_writes_a_file_and_reports_a_session() {
        let root = std::env::temp_dir().join(format!("mitten-cc-live-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create");
        let config = ClaudeCode {
            dirs: vec![root.clone()],
            permission_mode: "acceptEdits".to_owned(),
            allowed_tools: Vec::new(),
            timeout: std::time::Duration::from_secs(300),
            command: "claude".to_owned(),
        };
        let mut io = Recorder::default();
        let args = json!({
            "task": "Create a file named hello.txt containing exactly the word hi. Nothing else.",
            "dir": root,
        });
        let out = run(&config, &args, &mut io).await.expect("runs");
        assert!(
            out.starts_with("session: ") && !out.contains("error"),
            "{out}"
        );
        assert_eq!(io.asked, 1);
        assert!(
            io.notes.iter().any(|n| n.starts_with("🤖 ")),
            "no steps shown"
        );
        let written = std::fs::read_to_string(root.join("hello.txt")).expect("file written");
        assert_eq!(written.trim(), "hi");
        std::fs::remove_dir_all(&root).expect("clean up");
    }

    #[tokio::test]
    async fn resolve_allows_only_listed_directories() {
        let root = std::env::temp_dir().join(format!("mitten-cc-{}", std::process::id()));
        std::fs::create_dir_all(root.join("repo/sub")).expect("create");
        std::fs::create_dir_all(root.join("other")).expect("create");
        let config = ClaudeCode {
            dirs: vec![root.join("repo")],
            permission_mode: "acceptEdits".to_owned(),
            allowed_tools: Vec::new(),
            timeout: std::time::Duration::from_secs(1),
            command: "claude".to_owned(),
        };
        let ok = resolve(&config, &json!(root.join("repo/sub"))).await;
        assert!(ok.is_ok(), "{ok:?}");
        assert!(resolve(&config, &json!(root.join("other"))).await.is_err());
        assert!(
            resolve(&config, &json!(root.join("repo/../other")))
                .await
                .is_err()
        );
        std::fs::remove_dir_all(&root).expect("clean up");
    }
}

//! MCP client: connects to the configured servers at startup and offers their tools to the model
//! as `<server>__<tool>`. Each call asks the user first unless the server sets `approve = false`.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use rig_core::completion::ToolDefinition;
use rmcp::ServiceExt as _;
use rmcp::model::{CallToolRequestParams, ContentBlock, Tool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use tokio::io::AsyncBufReadExt as _;

use crate::agent::Io;
use crate::config::McpServer;

/// How long a server gets to start and list its tools; `npx` may download the server first.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest tool name providers accept.
const MAX_NAME_CHARS: usize = 64;
// ponytail: hard cap on tool output fed back to the model; page it if servers return more.
const MAX_OUTPUT_CHARS: usize = 30_000;

/// The connected servers; ones that failed to start are left out.
#[derive(Default)]
pub struct Mcp {
    servers: Vec<Server>,
    /// Every server's tools, as sent to the model.
    definitions: Vec<ToolDefinition>,
}

struct Server {
    name: String,
    approve: bool,
    client: RunningService<RoleClient, ()>,
    /// Name the model uses, and the server's own tool name.
    tools: Vec<(String, String)>,
}

impl std::fmt::Debug for Mcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.servers.iter().map(|s| s.name.as_str()).collect();
        f.debug_struct("Mcp").field("servers", &names).finish()
    }
}

impl Mcp {
    /// Starts every server at once; a server that fails is logged and skipped.
    pub async fn connect(servers: &BTreeMap<String, McpServer>) -> Self {
        let started = futures::future::join_all(servers.iter().map(|(name, config)| async move {
            let started = tokio::time::timeout(CONNECT_TIMEOUT, start(name, config)).await;
            match started {
                Ok(Ok(server)) => Some(server),
                Ok(Err(err)) => {
                    tracing::warn!("MCP server {name} failed to start: {err:#}");
                    None
                }
                Err(_) => {
                    tracing::warn!("MCP server {name} did not start within {CONNECT_TIMEOUT:?}");
                    None
                }
            }
        }))
        .await;
        let mut mcp = Self::default();
        for (server, definitions) in started.into_iter().flatten() {
            tracing::info!(
                "MCP server {} offers {} tools",
                server.name,
                server.tools.len()
            );
            mcp.definitions.extend(definitions);
            mcp.servers.push(server);
        }
        mcp
    }

    pub fn tools(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    /// Runs the MCP tool the model called `name`, or returns `None` if no server offers it.
    /// Failures of the tool itself come back as text for the model.
    pub async fn call(&self, name: &str, args: &Value, io: &mut impl Io) -> Option<Result<String>> {
        let (server, tool) = self.servers.iter().find_map(|server| {
            let (_, tool) = server.tools.iter().find(|(exposed, _)| exposed == name)?;
            Some((server, tool))
        })?;
        Some(server.call(tool, args, io).await)
    }
}

impl Server {
    async fn call(&self, tool: &str, args: &Value, io: &mut impl Io) -> Result<String> {
        let shown = format!(
            "{} → {tool}\n{}",
            self.name,
            serde_json::to_string_pretty(args).unwrap_or_default()
        );
        if self.approve {
            if !io.confirm(&shown).await? {
                return Ok("error: the user denied this call".to_owned());
            }
        } else {
            io.note(&format!("🔌 {} → {tool}", self.name)).await?;
        }
        let mut params = CallToolRequestParams::new(tool.to_owned());
        if let Value::Object(args) = args {
            params = params.with_arguments(args.clone());
        }
        let (output, ok) = match self.client.call_tool(params).await {
            Ok(result) => {
                let mut text: Vec<String> = result.content.iter().map(describe).collect();
                if text.is_empty()
                    && let Some(structured) = &result.structured_content
                {
                    text.push(structured.to_string());
                }
                (truncate(text.join("\n")), result.is_error != Some(true))
            }
            Err(err) => (format!("{err:#}"), false),
        };
        if self.approve {
            io.ran(&output, ok).await?;
        }
        Ok(if ok {
            output
        } else {
            format!("error: {output}")
        })
    }
}

/// Connects to one server and lists its tools.
async fn start(name: &str, config: &McpServer) -> Result<(Server, Vec<ToolDefinition>)> {
    let client = match (&config.command, &config.url) {
        (Some(command), _) => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(&config.args)
                .envs(config.env.iter().map(|(k, v)| (k, v.as_str())));
            let (transport, stderr) = TokioChildProcess::builder(cmd)
                .stderr(Stdio::piped())
                .spawn()
                .with_context(|| format!("failed to run {command}"))?;
            // The chat screen owns the terminal, so the server's stderr goes to the log instead.
            if let Some(stderr) = stderr {
                let name = name.to_owned();
                tokio::spawn(async move {
                    let mut lines = tokio::io::BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        tracing::info!(server = %name, "{line}");
                    }
                });
            }
            ().serve(transport).await.context("MCP handshake failed")?
        }
        (None, Some(url)) => {
            let mut transport = StreamableHttpClientTransportConfig::with_uri(url.as_str());
            if let Some(token) = &config.token {
                transport = transport.auth_header(token.as_str());
            }
            let transport =
                StreamableHttpClientTransport::with_client(reqwest::Client::new(), transport);
            ().serve(transport).await.context("MCP handshake failed")?
        }
        (None, None) => anyhow::bail!("needs `command` or `url`"),
    };
    let listed = client
        .list_all_tools()
        .await
        .context("failed to list tools")?;
    let mut tools = Vec::new();
    let mut definitions = Vec::new();
    for tool in listed {
        let exposed = exposed_name(name, &tool.name);
        if exposed.chars().count() > MAX_NAME_CHARS {
            tracing::warn!("skipping MCP tool {exposed}: name over {MAX_NAME_CHARS} characters");
            continue;
        }
        definitions.push(definition(&exposed, &tool));
        tools.push((exposed, tool.name.into_owned()));
    }
    let server = Server {
        name: name.to_owned(),
        approve: config.approve,
        client,
        tools,
    };
    Ok((server, definitions))
}

/// `<server>__<tool>`, with characters providers reject in tool names turned into `_`.
fn exposed_name(server: &str, tool: &str) -> String {
    let tool: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{server}__{tool}")
}

fn definition(exposed: &str, tool: &Tool) -> ToolDefinition {
    ToolDefinition {
        name: exposed.to_owned(),
        description: tool
            .description
            .as_deref()
            .or(tool.title.as_deref())
            .unwrap_or_default()
            .to_owned(),
        parameters: Value::Object((*tool.input_schema).clone()),
    }
}

/// Text of one result block; other kinds are named, since only text goes back to the model.
fn describe(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(_) => "[image omitted]".to_owned(),
        ContentBlock::Audio(_) => "[audio omitted]".to_owned(),
        ContentBlock::Resource(resource) => {
            serde_json::to_string(&resource.resource).unwrap_or_default()
        }
        ContentBlock::ResourceLink(link) => format!("[resource: {}]", link.uri),
        _ => "[unsupported content omitted]".to_owned(),
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
    fn exposed_names_are_safe_for_providers() {
        assert_eq!(exposed_name("gh", "create_issue"), "gh__create_issue");
        assert_eq!(exposed_name("fs", "read.file/v2"), "fs__read_file_v2");
    }

    /// Approves everything and records what it was shown.
    #[derive(Default)]
    struct Approver(Vec<String>);

    impl Io for Approver {
        async fn say(&mut self, _text: &str) -> Result<()> {
            Ok(())
        }
        async fn confirm(&mut self, action: &str) -> Result<bool> {
            self.0.push(action.to_owned());
            Ok(true)
        }
    }

    /// Needs node and network: `cargo test -- --ignored everything_server`.
    #[tokio::test]
    #[ignore = "starts a real MCP server through npx"]
    async fn everything_server_round_trip() {
        let config = crate::config::Config::parse(
            "[opencode-go]\napi_key = \"k\"\n[mcp.servers.every]\ncommand = \"npx\"\n\
             args = [\"-y\", \"@modelcontextprotocol/server-everything\"]\n",
        )
        .expect("valid");
        let mcp = Mcp::connect(&config.mcp).await;
        assert!(mcp.tools().iter().any(|t| t.name == "every__echo"));
        let mut io = Approver::default();
        let out = mcp
            .call(
                "every__echo",
                &serde_json::json!({"message": "hi"}),
                &mut io,
            )
            .await
            .expect("tool exists")
            .expect("call runs");
        assert!(out.contains("hi") && !out.starts_with("error"), "{out}");
        assert_eq!(io.0.len(), 1, "asked for approval once");
        assert!(
            mcp.call("every__nope", &Value::Null, &mut io)
                .await
                .is_none()
        );
    }

    #[test]
    fn truncate_caps_output() {
        assert_eq!(truncate("ok".to_owned()), "ok");
        assert!(truncate("é".repeat(MAX_OUTPUT_CHARS + 1)).ends_with("[output truncated]"));
    }
}

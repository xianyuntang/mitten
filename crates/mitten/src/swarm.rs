//! `swarm` tool: the calling model acts as an orchestrator and dispatches a wave of subagents, each
//! with its own role, task, and history, that work in parallel with the read-only web and file
//! tools and report back. The orchestrator reads the reports and sends further waves as needed.

use anyhow::Result;
use futures::StreamExt;
use rig_core::completion::{CompletionRequest, ToolDefinition};
use rig_core::message::{AssistantContent, Message, ToolCall, ToolResultContent, UserContent};
use serde_json::{Value, json};

use crate::agent::{Io, Model, request};
use crate::config::Config;
use crate::fetch::{self, Fetcher};
use crate::files;
use crate::search::{self, WebSearch};

/// Subagents one call may dispatch; more go in another wave.
const MAX_AGENTS: usize = 100;
/// Subagents running at once; the rest wait their turn.
// ponytail: fixed; make it a config key if a provider's rate limit needs it lower or allows more.
const CONCURRENCY: usize = 8;
/// Model calls one subagent makes before it must report.
const AGENT_STEPS: usize = 8;
/// Characters kept from each report, so a full wave fits the orchestrator's context.
const MAX_REPORT_CHARS: usize = 1_500;

/// System prompt section, included only when `swarm` is available.
pub const GUIDANCE: &str = "\
# Swarm
For large work that splits into independent parts (broad research, comparing many products or \
sources, checking a long list of items, surveying a field), act as an orchestrator: call swarm \
with many subagents at once instead of doing the parts yourself one by one. Scale the swarm to \
the job: a broad survey can use dozens to 100 subagents in one wave. Give each a short name, a \
role, and one narrow, self-contained task with all the context it needs, since subagents see \
nothing but their task. Read the reports, then send another wave for gaps, follow-ups, or fact \
checks if needed, and finally write the answer yourself, resolving disagreements and naming the \
URLs it relies on. Don't use swarm for quick questions or for work where each step depends on the \
last; do those yourself.";

const AGENT: &str = "\
You are a subagent in a swarm; an orchestrator gave you one task and will combine your report \
with others. Do only your task, using web_search and fetch_url (and read_file or list_dir for \
local files). Prefer primary and official sources, check dates, and read promising pages rather \
than relying on snippets. Reply with a report under 200 words: the findings, the source URL after \
each claim, and what you could not find or verify. Never fabricate.";

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "swarm".to_owned(),
        description: format!(
            "Dispatch a wave of up to {MAX_AGENTS} subagents that work in parallel, each searching \
             the web and reading pages or local files for its own task, and return their reports. \
             Subagents only read; they cannot spawn subagents."
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "agents": {
                    "type": "array",
                    "maxItems": MAX_AGENTS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string", "description": "Short label, e.g. PricingResearcher."},
                            "role": {"type": "string", "description": "Who the subagent is and how it should work."},
                            "task": {"type": "string", "description": "One self-contained task with all needed context."},
                        },
                        "required": ["task"],
                    },
                },
            },
            "required": ["agents"],
        }),
    }
}

/// What a subagent may use: the web and local files, read-only.
#[derive(Debug, Clone, Copy)]
pub struct Tools<'a> {
    pub config: &'a Config,
    pub search: &'a WebSearch,
    pub fetcher: &'a Fetcher,
}

#[derive(Debug, PartialEq)]
struct Agent {
    name: String,
    role: String,
    task: String,
}

/// Runs one swarm tool call. `now` is the current time, so subagents judge how recent a source
/// is. A subagent's model failure becomes its report; only `io` errors fail the call.
pub async fn run(
    model: &Model,
    tools: Tools<'_>,
    now: &str,
    args: &Value,
    io: &mut impl Io,
) -> Result<String> {
    let agents = match parse(args) {
        Ok(agents) => agents,
        Err(problem) => return Ok(format!("error: {problem}")),
    };
    let total = agents.len();
    io.note(&format!(
        "🐝 swarm: {total} subagents, {CONCURRENCY} at a time"
    ))
    .await?;
    let mut running = futures::stream::iter(agents.into_iter().enumerate())
        .map(|(index, agent)| async move {
            let report = work(model, tools, now, &agent).await;
            (index, agent.name, report)
        })
        .buffer_unordered(CONCURRENCY);
    // Report about every tenth of the swarm, so a big one doesn't flood the chat.
    let every = total.div_ceil(10);
    let mut reports = Vec::with_capacity(total);
    while let Some(finished) = running.next().await {
        reports.push(finished);
        let done = reports.len();
        if done % every == 0 || done == total {
            io.note(&format!("🐝 {done}/{total} done")).await?;
        }
    }
    reports.sort_by_key(|(index, ..)| *index);
    Ok(reports
        .into_iter()
        .map(|(_, name, report)| format!("## {name}\n{}", clip(report.trim())))
        .collect::<Vec<_>>()
        .join("\n\n"))
}

fn parse(args: &Value) -> Result<Vec<Agent>, String> {
    let list = args["agents"]
        .as_array()
        .filter(|list| !list.is_empty())
        .ok_or("`agents` must be a non-empty array")?;
    if list.len() > MAX_AGENTS {
        return Err(format!(
            "at most {MAX_AGENTS} subagents per call; send the rest in another wave"
        ));
    }
    list.iter()
        .enumerate()
        .map(|(index, agent)| {
            let field = |key: &str| agent[key].as_str().map(str::trim).unwrap_or_default();
            let task = field("task");
            if task.is_empty() {
                return Err(format!("subagent {} has no task", index + 1));
            }
            let name = match field("name") {
                "" => format!("agent-{}", index + 1),
                name => name.to_owned(),
            };
            Ok(Agent {
                name,
                role: field("role").to_owned(),
                task: task.to_owned(),
            })
        })
        .collect()
}

/// One subagent: a small tool loop over its own history, ending in a text report.
async fn work(model: &Model, tools: Tools<'_>, now: &str, agent: &Agent) -> String {
    let system = if agent.role.is_empty() {
        AGENT.to_owned()
    } else {
        format!("{AGENT}\n\nYour role: {}", agent.role)
    };
    let mut messages = vec![Message::user(format!(
        "Today is {now}.\nYour task: {}",
        agent.task
    ))];
    let mut last = String::new();
    for step in 1..=AGENT_STEPS {
        let response = match model
            .complete(agent_request(tools.config, &system, &messages))
            .await
        {
            Ok(response) => response,
            Err(err) => return format!("error: model request failed: {err:#}"),
        };
        let mut calls: Vec<ToolCall> = Vec::new();
        last.clear();
        for content in &response.choice {
            match content {
                AssistantContent::Text(text) => last.push_str(text.text()),
                AssistantContent::ToolCall(call) => calls.push(call.clone()),
                _ => {}
            }
        }
        messages.push(Message::Assistant {
            id: response.message_id.clone(),
            content: response.choice,
        });
        if calls.is_empty() {
            return last;
        }
        let mut results = Vec::new();
        for call in &calls {
            let args = &call.function.arguments;
            let output = match call.function.name.as_str() {
                "web_search" => tools.search.run(args).await,
                "fetch_url" => tools.fetcher.run(args).await,
                "read_file" => files::read(tools.config, args).await,
                "list_dir" => files::list(tools.config, args).await,
                other => format!("error: unknown tool `{other}`"),
            };
            results.push(UserContent::tool_result_for(
                call.id.clone(),
                call.provider.clone(),
                call.function.name.clone(),
                vec![ToolResultContent::text(output)],
            ));
        }
        if step + 1 == AGENT_STEPS {
            results.push(UserContent::text(
                "This is your last step: write your report now, without more tool calls.",
            ));
        }
        messages.push(Message::User { content: results });
    }
    format!("{last}\n(stopped at the step limit)")
}

fn agent_request(config: &Config, system: &str, messages: &[Message]) -> CompletionRequest {
    let mut request = request(config, system, messages, &[]);
    request.tools = vec![
        search::tool(),
        fetch::tool(),
        files::read_tool(),
        files::list_tool(),
    ];
    request
}

/// `report` cut to `MAX_REPORT_CHARS`, marked when cut.
fn clip(report: &str) -> String {
    match report.char_indices().nth(MAX_REPORT_CHARS) {
        Some((end, _)) => format!("{}… (cut)", &report[..end]),
        None => report.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_names_agents_and_rejects_bad_waves() {
        let agents = parse(&json!({"agents": [
            {"name": " Pricing ", "role": "analyst", "task": " compare plans "},
            {"task": "check dates"},
        ]}))
        .unwrap_or_default();
        assert_eq!(
            agents,
            [
                Agent {
                    name: "Pricing".into(),
                    role: "analyst".into(),
                    task: "compare plans".into()
                },
                Agent {
                    name: "agent-2".into(),
                    role: String::new(),
                    task: "check dates".into()
                },
            ]
        );
        assert!(parse(&json!({"agents": []})).is_err());
        assert!(parse(&json!({"agents": [{"name": "x"}]})).is_err());
        let too_many = vec![json!({"task": "t"}); MAX_AGENTS + 1];
        assert!(parse(&json!({ "agents": too_many })).is_err());
    }

    #[test]
    fn clip_cuts_on_char_boundary() {
        let long = "字".repeat(MAX_REPORT_CHARS + 5);
        let clipped = clip(&long);
        assert!(clipped.ends_with("… (cut)"));
        assert_eq!(
            clipped.chars().count(),
            MAX_REPORT_CHARS + "… (cut)".chars().count()
        );
        assert_eq!(clip("short"), "short");
    }
}

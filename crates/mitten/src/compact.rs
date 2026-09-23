//! Conversation compaction: when history grows past a token budget, older turns are replaced by a
//! model-written summary so requests stay within the model's context window.

use rig_core::message::{AssistantContent, Message, ToolResultContent, UserContent};

/// Instructions for the summarizing request.
pub const SYSTEM: &str = "\
You compress conversation history. Summarize the conversation you are given so an assistant can \
continue it without the original. Keep: the user's goals and requests, decisions and stated \
preferences, facts learned about the machine and environment (paths, versions, settings), commands \
run and their important results, errors and how they were resolved, and open tasks or next steps. \
Drop pleasantries and output that no longer matters. Write in the conversation's language, as \
concise bullet points under short headings, in under 1500 words. Output only the summary.";

/// Opens the replacement history; the summary is framed as context, not as a new request.
const SUMMARY_HEADER: &str = "[Summary of the earlier conversation, written when it was compacted]";
/// Tool output characters kept per result in the transcript sent for summarizing.
const MAX_TOOL_OUTPUT_CHARS: usize = 2_000;

/// Rough token count: serialized characters / 3, between English (~4 per token) and CJK (~1-2).
// ponytail: heuristic; switch to the provider's reported usage if the estimate misfires.
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| serde_json::to_string(m).map_or(0, |s| s.chars().count()))
        .sum::<usize>()
        / 3
}

/// A user message that starts a turn, as opposed to one carrying tool results.
fn starts_turn(message: &Message) -> bool {
    matches!(message, Message::User { content }
        if content.iter().all(|c| !matches!(c, UserContent::ToolResult(_))))
}

/// Where to split `messages` so the kept tail is whole turns within `keep_tokens`. Everything before
/// the returned index gets summarized; it is `messages.len()` when even the last turn is too big.
pub fn split_point(messages: &[Message], keep_tokens: usize) -> usize {
    (0..messages.len())
        .filter(|&i| starts_turn(&messages[i]))
        .find(|&i| estimate_tokens(&messages[i..]) <= keep_tokens)
        .unwrap_or(messages.len())
}

/// Plain-text rendering of `messages` for the summarizer, with long tool output cut.
pub fn transcript(messages: &[Message]) -> String {
    let mut out = Vec::new();
    for message in messages {
        match message {
            Message::System { .. } => {}
            Message::User { content } => {
                for part in content {
                    match part {
                        UserContent::Text(text) => out.push(format!("User: {}", text.text())),
                        UserContent::ToolResult(result) => {
                            let text: String = result
                                .content
                                .iter()
                                .filter_map(ToolResultContent::as_text)
                                .collect::<Vec<_>>()
                                .join("\n");
                            let cut: String = text.chars().take(MAX_TOOL_OUTPUT_CHARS).collect();
                            let more = if cut.len() < text.len() { " […]" } else { "" };
                            out.push(format!("Tool result ({}): {cut}{more}", result.name));
                        }
                        _ => out.push("User: [attachment]".to_owned()),
                    }
                }
            }
            Message::Assistant { content, .. } => {
                for part in content {
                    match part {
                        AssistantContent::Text(text) => {
                            out.push(format!("Assistant: {}", text.text()));
                        }
                        AssistantContent::ToolCall(call) => out.push(format!(
                            "Assistant called {}: {}",
                            call.function.name, call.function.arguments
                        )),
                        _ => {}
                    }
                }
            }
        }
    }
    out.join("\n\n")
}

/// The two messages that stand in for everything summarized.
pub fn summary_messages(summary: &str) -> [Message; 2] {
    [
        Message::user(format!("{SUMMARY_HEADER}\n\n{}", summary.trim())),
        Message::assistant("Understood. I'll continue from this summary."),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(prompt: &str, reply: &str) -> [Message; 2] {
        [Message::user(prompt), Message::assistant(reply)]
    }

    #[test]
    fn split_point_keeps_whole_recent_turns_within_budget() {
        let mut messages = Vec::new();
        messages.extend(turn(&"old ".repeat(300), "a"));
        messages.extend(turn("recent", "b"));
        messages.extend(turn("latest", "c"));
        let recent = estimate_tokens(&messages[2..]);
        assert_eq!(split_point(&messages, recent), 2);
        assert_eq!(split_point(&messages, estimate_tokens(&messages)), 0);
        assert_eq!(split_point(&messages, 1), messages.len());
    }

    #[test]
    fn transcript_names_speakers() {
        let text = transcript(&turn("hi", "hello"));
        assert_eq!(text, "User: hi\n\nAssistant: hello");
        let [summary, ack] = summary_messages(" s ");
        assert!(starts_turn(&summary));
        assert!(matches!(ack, Message::Assistant { .. }));
    }
}

//! Long-term memory: short facts the agent saves itself, shared by every conversation (the terminal
//! and every Discord channel) and loaded whenever a conversation starts, even after `/new`.

use serde_json::{Value, json};

use rig_core::completion::ToolDefinition;

use crate::db::Memory;

/// Total characters across all entries; everything here rides along in every request.
// ponytail: fixed budget; make it configurable if people hit it.
pub const CHAR_LIMIT: usize = 2_200;

/// Stable guidance for the system prompt; the entries themselves come from `snapshot`.
pub const GUIDANCE: &str = "\
# Memory
You have long-term memory shared by every conversation (the terminal and every Discord channel); \
it survives /new and is loaded whenever a conversation starts. \
Save with the memory tool facts that matter from now on: who you are talking to, their \
preferences, names or roles they give you, stable facts about this machine, standing conventions. \
When the user asks you to remember something, or tells you such a fact, call the memory tool in \
that same response. Never say you remembered or noted something unless the memory tool call \
succeeded; replying \"noted\" without the call saves nothing. \
Skip task progress, things that are easy to rediscover, and anything stale within a week. \
Write declarative facts (\"User prefers short answers\"), not instructions to yourself. \
When memory is full, replace or merge stale entries instead of skipping the save.";

/// A change the memory tool asked for, already validated against the current entries.
#[derive(Debug, PartialEq, Eq)]
pub enum Edit {
    Add(String),
    Replace(i64, String),
    Remove(i64),
}

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "memory".to_owned(),
        description: format!(
            "Add, replace, or remove a long-term memory entry, shared by every conversation. Entries \
             load every time one starts, so keep them short and high-signal. Total budget: {CHAR_LIMIT} characters."
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "replace", "remove"]},
                "content": {
                    "type": "string",
                    "description": "The entry text, for add and replace. For replace this is the whole new entry.",
                },
                "old_text": {
                    "type": "string",
                    "description": "For replace and remove: a short substring that identifies exactly one existing entry.",
                },
            },
            "required": ["action"],
        }),
    }
}

/// The saved entries as they appear in the system prompt.
pub fn snapshot(entries: &[Memory]) -> String {
    if entries.is_empty() {
        return "Saved facts: none yet. You have not met this user here. Before anything else, \
                briefly introduce yourself and ask who they are and what to call them; hold off on \
                their request until they answer. When they do, save it with the memory tool, then \
                carry on with what they first asked."
            .to_owned();
    }
    let list: Vec<String> = entries.iter().map(|e| format!("- {}", e.content)).collect();
    format!("Saved facts:\n{}", list.join("\n"))
}

/// Validates a memory tool call against `entries`. Returns the edit and the characters used after it,
/// or a message for the model explaining what to fix.
pub fn plan(entries: &[Memory], args: &Value) -> Result<(Edit, usize), String> {
    let content = args["content"]
        .as_str()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let needs_content = || {
        content
            .map(str::to_owned)
            .ok_or("`content` is required for add and replace")
    };
    let edit = match args["action"].as_str().unwrap_or_default() {
        "add" => Edit::Add(needs_content()?),
        "replace" => Edit::Replace(target(entries, args)?, needs_content()?),
        "remove" => Edit::Remove(target(entries, args)?),
        other => {
            return Err(format!(
                "unknown action {other:?}; use add, replace, or remove"
            ));
        }
    };
    let (dropped, added) = match &edit {
        Edit::Add(text) => (None, text.chars().count()),
        Edit::Replace(id, text) => (Some(*id), text.chars().count()),
        Edit::Remove(id) => (Some(*id), 0),
    };
    let used = added
        + entries
            .iter()
            .filter(|e| Some(e.id) != dropped)
            .map(|e| e.content.chars().count())
            .sum::<usize>();
    if used > CHAR_LIMIT {
        return Err(format!(
            "memory is full: this would use {used}/{CHAR_LIMIT} characters. \
             Replace or remove stale entries first.\n{}",
            snapshot(entries)
        ));
    }
    Ok((edit, used))
}

/// The one entry whose text contains `old_text`.
fn target(entries: &[Memory], args: &Value) -> Result<i64, String> {
    let needle = args["old_text"]
        .as_str()
        .filter(|t| !t.trim().is_empty())
        .ok_or("`old_text` is required for replace and remove")?;
    let found: Vec<&Memory> = entries
        .iter()
        .filter(|e| e.content.contains(needle))
        .collect();
    match found.as_slice() {
        [one] => Ok(one.id),
        [] => Err(format!(
            "no entry contains {needle:?}\n{}",
            snapshot(entries)
        )),
        many => Err(format!(
            "{needle:?} matches {} entries; use a longer substring",
            many.len()
        )),
    }
}

/// One line telling the user what the agent just remembered or forgot.
pub fn describe(edit: &Edit, entries: &[Memory]) -> String {
    let old = |id: &i64| {
        entries
            .iter()
            .find(|e| e.id == *id)
            .map_or("", |e| e.content.as_str())
            .to_owned()
    };
    match edit {
        Edit::Add(text) => format!("🧠 remembered: {text}"),
        Edit::Replace(_, text) => format!("🧠 updated memory: {text}"),
        Edit::Remove(id) => format!("🧠 forgot: {}", old(id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, content: &str) -> Memory {
        Memory {
            id,
            content: content.to_owned(),
        }
    }

    #[test]
    fn plan_validates_targets_and_budget() {
        let entries = [
            entry(1, "User is Alex"),
            entry(2, "Server runs Ubuntu 24.04"),
        ];
        let add = json!({"action": "add", "content": " Prefers zh-TW "});
        assert_eq!(
            plan(&entries, &add),
            Ok((Edit::Add("Prefers zh-TW".to_owned()), 12 + 24 + 13))
        );

        let replace =
            json!({"action": "replace", "old_text": "Ubuntu", "content": "Server runs Debian 13"});
        assert_eq!(
            plan(&entries, &replace).map(|(edit, _)| edit),
            Ok(Edit::Replace(2, "Server runs Debian 13".to_owned()))
        );
        let remove = json!({"action": "remove", "old_text": "Alex"});
        assert_eq!(plan(&entries, &remove), Ok((Edit::Remove(1), 24)));

        assert!(
            plan(&entries, &json!({"action": "remove", "old_text": "er"})).is_err(),
            "ambiguous"
        );
        assert!(plan(&entries, &json!({"action": "remove", "old_text": "nope"})).is_err());
        assert!(plan(&entries, &json!({"action": "add"})).is_err());

        let huge = json!({"action": "add", "content": "x".repeat(CHAR_LIMIT)});
        assert!(
            plan(&entries, &huge)
                .unwrap_err()
                .starts_with("memory is full")
        );
    }
}

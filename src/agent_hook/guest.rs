//! Guests (BACKLOG 52 H4, owner ruling 2026-10-22): products that run another product's hook
//! entries with payloads of their own.
//!
//! Grok Build runs a repository's `.claude/settings.json` hooks (`tests/fixtures/grok-1.0.52`).
//! Its payload carries Claude Code's keys beside its own (`hookEventName` beside
//! `hook_event_name`), but its tool names are its own, so bay judged none of its calls: neither
//! the deny list, the stale-write
//! judgement, the claims nor the locks. A payload in Grok's shape (its own `hookEventName` beside
//! `workspaceRoot`) is read as Claude Code's, with its two recorded tool names put as Claude
//! Code's: `run_terminal_command` is the shell (`Bash`, `tool_input.command`) and `write` an edit
//! (`Write`, `tool_input.file_path`). No field the recordings do not show is read (R16); any other
//! tool name stays as it is, a call bay does not judge.

use serde_json::Value;

/// The name a session of Grok is shown under (BACKLOG 61 a).
pub(super) const GROK: &str = "grok";

/// Grok's tool names, and Claude Code's names for the same calls.
const GROK_TOOLS: [(&str, &str); 2] = [("run_terminal_command", "Bash"), ("write", "Write")];

/// `input` as Claude Code's payload when it is a guest's in a shape bay reads; `None` when it is
/// no guest's (or not JSON), so it is read as it is.
pub(super) fn as_claude(input: &str) -> Option<String> {
    let mut value: Value = serde_json::from_str(input).ok()?;
    if !is_grok(&value) {
        return None;
    }
    for key in ["tool_name", "toolName"] {
        let Some(name) = value.get(key).and_then(Value::as_str) else {
            continue;
        };
        if let Some((_, claude)) = GROK_TOOLS.iter().find(|(grok, _)| *grok == name) {
            value[key] = Value::from(*claude);
        }
    }
    Some(value.to_string())
}

/// Grok's own keys, beside Claude Code's: `workspaceRoot` and a camelCase `hookEventName`.
fn is_grok(value: &Value) -> bool {
    value.get("workspaceRoot").is_some_and(Value::is_string)
        && value.get("hookEventName").is_some_and(Value::is_string)
}

#[cfg(test)]
mod tests {
    use super::as_claude;

    #[test]
    fn grok_tool_names_read_as_claude_codes_and_others_stay() {
        let shell = r#"{"hookEventName":"pre_tool_use","workspaceRoot":"/w","tool_name":"run_terminal_command","toolName":"run_terminal_command","tool_input":{"command":"ls"}}"#;
        let read: serde_json::Value = serde_json::from_str(&as_claude(shell).unwrap()).unwrap();
        assert_eq!(read["tool_name"], "Bash");
        let other = r#"{"hookEventName":"pre_tool_use","workspaceRoot":"/w","tool_name":"grep"}"#;
        let read: serde_json::Value = serde_json::from_str(&as_claude(other).unwrap()).unwrap();
        assert_eq!(read["tool_name"], "grep");
        assert!(as_claude(r#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#).is_none());
    }
}

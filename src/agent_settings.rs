//! bay's entries in the project agent settings (`DESIGN` § 11a, § 11b, § 15).
//!
//! For Claude Code the project settings file is `.claude/settings.json`. bay owns only the
//! matcher groups whose every hook command is `bay hook <event>`; every other entry is
//! another tool's and is never removed, reordered or de-duplicated.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Claude Code events bay hooks (§ 11a), with the `bay hook` event name each one calls.
pub const CLAUDE_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "user-prompt-submit"),
    ("PreToolUse", "pre-tool"),
    ("PostToolBatch", "post-tool"),
    ("FileChanged", "file-changed"),
    ("CwdChanged", "cwd-changed"),
    ("WorktreeCreate", "worktree-create"),
    ("WorktreeRemove", "worktree-remove"),
    ("Notification", "notification"),
    ("Stop", "stop"),
    ("SubagentStart", "subagent-start"),
    ("SubagentStop", "subagent-stop"),
    ("StopFailure", "stop-failure"),
    ("SessionEnd", "session-end"),
    ("PreCompact", "pre-compact"),
];

const BAY_COMMAND_PREFIX: &str = "bay hook ";

#[must_use]
pub fn claude_settings_path(root: &Path) -> PathBuf {
    root.join(".claude").join("settings.json")
}

/// Reads the settings file. A missing file is an empty object.
///
/// # Errors
///
/// The file exists but is not a JSON object.
pub fn read(path: &Path) -> Result<Map<String, Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    match serde_json::from_str(&text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(format!("{}: not a JSON object", path.display())),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn is_bay_group(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            !hooks.is_empty()
                && hooks.iter().all(|hook| {
                    hook.get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|command| command.starts_with(BAY_COMMAND_PREFIX))
                })
        })
}

fn bay_group(event: &str, bay_event: &str) -> Value {
    let mut hook =
        json!({"type": "command", "command": format!("{BAY_COMMAND_PREFIX}{bay_event}")});
    if event == "PreToolUse" {
        // § 11a: exec form with `onFailure: "block"`, probed on the installed version (ROADMAP § 4).
        hook["onFailure"] = json!("block");
        return json!({"matcher": "Edit|Write|NotebookEdit|Bash", "hooks": [hook]});
    }
    json!({"hooks": [hook]})
}

/// The Claude events that have no bay entry.
#[must_use]
pub fn missing_claude_events(settings: &Map<String, Value>) -> Vec<&'static str> {
    let hooks = settings.get("hooks").and_then(Value::as_object);
    CLAUDE_EVENTS
        .iter()
        .filter(|(event, bay_event)| {
            let wanted = format!("{BAY_COMMAND_PREFIX}{bay_event}");
            let present = hooks
                .and_then(|hooks| hooks.get(*event))
                .and_then(Value::as_array)
                .is_some_and(|groups| {
                    groups
                        .iter()
                        .filter(|group| is_bay_group(group))
                        .any(|group| {
                            group["hooks"].as_array().is_some_and(|hooks| {
                                hooks.iter().any(|hook| hook["command"] == wanted.as_str())
                            })
                        })
                });
            !present
        })
        .map(|(event, _)| *event)
        .collect()
}

/// A digest over every hook entry that is not bay's, in file order.
#[must_use]
pub fn unrelated_digest(settings: &Map<String, Value>) -> String {
    let mut unrelated = Map::new();
    if let Some(hooks) = settings.get("hooks").and_then(Value::as_object) {
        for (event, groups) in hooks {
            let kept: Vec<&Value> = groups
                .as_array()
                .map(|groups| groups.iter().filter(|group| !is_bay_group(group)).collect())
                .unwrap_or_default();
            if !kept.is_empty() {
                unrelated.insert(event.clone(), json!(kept));
            }
        }
    }
    let bytes = serde_json::to_vec(&unrelated).unwrap_or_default();
    hex(&Sha256::digest(&bytes))
}

/// Replaces bay's groups for every Claude event and leaves every other entry where it was.
pub fn add_claude_entries(settings: &mut Map<String, Value>) {
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    let Some(hooks) = hooks.as_object_mut() else {
        return;
    };
    for (event, bay_event) in CLAUDE_EVENTS {
        let groups = hooks
            .entry(*event)
            .or_insert_with(|| Value::Array(Vec::new()));
        if !groups.is_array() {
            *groups = Value::Array(Vec::new());
        }
        if let Some(groups) = groups.as_array_mut() {
            groups.retain(|group| !is_bay_group(group));
            groups.push(bay_group(event, bay_event));
        }
    }
}

/// Writes the settings as two-space JSON with a trailing newline.
///
/// # Errors
///
/// The directory or file cannot be written.
pub fn write(path: &Path, settings: &Map<String, Value>) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    }
    let mut text = serde_json::to_string_pretty(settings).map_err(|error| error.to_string())?;
    text.push('\n');
    let tmp = path.with_extension("json.bay-tmp");
    std::fs::write(&tmp, text).map_err(|error| format!("{}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Lower-case hex of `bytes`.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

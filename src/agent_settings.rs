//! bay's entries in the project agent settings (`DESIGN` § 11a, § 11b, § 15).
//!
//! For Claude Code the project settings file is `.claude/settings.json`. bay owns only the
//! matcher groups whose every hook command is `bay hook <event>`, behind bay's presence test
//! ([`hook_command`]) or bare as an earlier bay wrote it; every other entry is another tool's
//! and is never removed, reordered or de-duplicated.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Claude Code events bay hooks (§ 11a), with the `bay hook` event name each one calls. Only
/// probed events (ruling R16): an installed hook can itself be behaviour (a `WorktreeCreate`
/// hook replaces the product's own worktree creation), so `CwdChanged`, `WorktreeCreate`,
/// `WorktreeRemove` and `StopFailure` wait for their probe.
pub const CLAUDE_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "user-prompt-submit"),
    ("PreToolUse", "pre-tool"),
    ("PostToolBatch", "post-tool"),
    ("FileChanged", "file-changed"),
    ("Notification", "notification"),
    ("Stop", "stop"),
    ("SubagentStart", "subagent-start"),
    ("SubagentStop", "subagent-stop"),
    // A teammate session's idle (probed, round 2): recorded as idle.
    ("TeammateIdle", "teammate-idle"),
    ("SessionEnd", "session-end"),
    ("PreCompact", "pre-compact"),
];

const BAY_COMMAND_PREFIX: &str = "bay hook ";

/// What bay's hook command runs before `bay hook <event>` (G169): on a box where `bay` does not
/// resolve on `PATH` the entry exits 0, so a settings file tracked in the repository never
/// turns `onFailure: "block"` into a deny of every tool on a box that runs no bay session.
/// Where bay resolves, `exec` hands the entry to it, so its exit code, a hang and its timeout
/// stay bay's and fail closed. POSIX sh: the product runs the command through its shell.
const PRESENCE_GUARD: &str = "command -v bay >/dev/null 2>&1 || exit 0; exec ";

/// The command bay installs for its `bay hook <event>` entry (G169).
#[must_use]
pub fn hook_command(bay_event: &str) -> String {
    format!("{PRESENCE_GUARD}{BAY_COMMAND_PREFIX}{bay_event}")
}

/// The `bay hook` event a hook command runs when it is bay's: the command bay installs, or the
/// bare `bay hook <event>` an earlier bay wrote, which a re-install replaces.
fn bay_event_of(command: &str) -> Option<&str> {
    command
        .strip_prefix(PRESENCE_GUARD)
        .unwrap_or(command)
        .strip_prefix(BAY_COMMAND_PREFIX)
        .filter(|event| !event.is_empty() && !event.contains(char::is_whitespace))
}

fn hook_runs(hook: &Value, bay_event: &str) -> bool {
    hook["command"]
        .as_str()
        .and_then(bay_event_of)
        .is_some_and(|event| event == bay_event)
}

/// The `timeout` (seconds) on bay's `PreToolUse` entry (G158): bay's hook takes about half a
/// second, so a hook still running at 30 s is hung, and the product denies the call.
pub const PRE_TOOL_TIMEOUT_S: u64 = 30;

/// Claude Code's project settings file, relative to the work tree's top level.
pub const CLAUDE_SETTINGS_FILE: &str = ".claude/settings.json";

#[must_use]
pub fn claude_settings_path(root: &Path) -> PathBuf {
    root.join(".claude").join("settings.json")
}

/// The settings files Claude Code reads beside the project's that can switch bay's hooks off
/// (G154): the repository's `.claude/settings.local.json`, and the user settings file in the
/// product's config directory (`CLAUDE_CONFIG_DIR`, else `$HOME/.claude`, the rule the skill
/// follows).
#[must_use]
pub fn claude_override_paths(root: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let user = crate::skill::config_dir()
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".claude")))
        .map(|dir| dir.join("settings.json"));
    std::iter::once(root.join(".claude").join("settings.local.json"))
        .chain(user)
        .collect()
}

/// The configured agent product's hook-settings file, relative to the top level: the one file a
/// sparse view holds beside its declared paths, so a session started in the view runs bay's
/// hooks (H7, G72). None without an agent adapter.
#[must_use]
pub fn hook_settings_file(config: &crate::config::Config) -> Option<&'static str> {
    config.claude_adapter().then_some(CLAUDE_SETTINGS_FILE)
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
                        .and_then(bay_event_of)
                        .is_some()
                })
        })
}

fn bay_group(event: &str, bay_event: &str) -> Value {
    let mut hook = json!({"type": "command", "command": hook_command(bay_event)});
    if event == "PreToolUse" {
        // § 11a: exec form with `onFailure: "block"`, probed on the installed version (ROADMAP § 4).
        hook["onFailure"] = json!("block");
        // G158: under `onFailure: "block"` a timeout denies (probe P2a); without one the
        // product's default (600 s on 2.1.303, probe P2b) holds the agent on a hung bay.
        hook["timeout"] = json!(PRE_TOOL_TIMEOUT_S);
        return json!({"matcher": "Edit|Write|NotebookEdit|Bash", "hooks": [hook]});
    }
    json!({"hooks": [hook]})
}

/// bay's hooks under `event` that run `bay hook <bay_event>`, in groups bay owns.
fn bay_hooks<'a>(
    settings: &'a Map<String, Value>,
    event: &str,
    bay_event: &'a str,
) -> impl Iterator<Item = &'a Value> + 'a {
    settings
        .get("hooks")
        .and_then(|hooks| hooks.get(event))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|group| is_bay_group(group))
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .filter(move |hook| hook_runs(hook, bay_event))
}

/// The Claude events that have no bay entry.
#[must_use]
pub fn missing_claude_events(settings: &Map<String, Value>) -> Vec<&'static str> {
    CLAUDE_EVENTS
        .iter()
        .filter(|(event, bay_event)| bay_hooks(settings, event, bay_event).next().is_none())
        .map(|(event, _)| *event)
        .collect()
}

/// The Claude events whose bay entry is the bare `bay hook <event>` an earlier bay wrote,
/// without the presence test (G169): on a box without bay it denies every tool. A re-install
/// replaces it.
#[must_use]
pub fn outdated_claude_events(settings: &Map<String, Value>) -> Vec<&'static str> {
    CLAUDE_EVENTS
        .iter()
        .filter(|(event, bay_event)| {
            let current = hook_command(bay_event);
            let mut entries = bay_hooks(settings, event, bay_event).peekable();
            entries.peek().is_some() && !entries.any(|hook| hook["command"] == current.as_str())
        })
        .map(|(event, _)| *event)
        .collect()
}

/// The Claude events bay hooks that `settings` gives hook entries of its own, none of them
/// bay's command (G154): in a settings file read beside the project's, such entries stand where
/// bay's should.
#[must_use]
pub fn claude_events_without_bay(settings: &Map<String, Value>) -> Vec<&'static str> {
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return Vec::new();
    };
    CLAUDE_EVENTS
        .iter()
        .filter(|(event, bay_event)| {
            hooks.get(*event).is_some_and(|groups| {
                !groups.as_array().is_some_and(|groups| {
                    groups
                        .iter()
                        .filter_map(|group| group["hooks"].as_array())
                        .flatten()
                        .any(|hook| hook_runs(hook, bay_event))
                })
            })
        })
        .map(|(event, _)| *event)
        .collect()
}

/// The `onFailure` of bay's `PreToolUse` entry: `None` when bay has no such entry, `Some(None)`
/// when the entry sets none. Claude Code reads `"block"` as: a hook that cannot start, exits
/// with a code other than 0 or 2, times out or prints invalid JSON stops the tool (H4).
#[must_use]
pub fn claude_pre_tool_on_failure(settings: &Map<String, Value>) -> Option<Option<String>> {
    claude_pre_tool_hook(settings).map(|hook| {
        hook.get("onFailure")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

/// The `timeout` (seconds) of bay's `PreToolUse` entry: `None` when bay has no such entry,
/// `Some(None)` when the entry sets none (G158).
#[must_use]
pub fn claude_pre_tool_timeout(settings: &Map<String, Value>) -> Option<Option<u64>> {
    claude_pre_tool_hook(settings).map(|hook| hook.get("timeout").and_then(Value::as_u64))
}

/// bay's `bay hook pre-tool` hook, in a `PreToolUse` group bay owns.
fn claude_pre_tool_hook(settings: &Map<String, Value>) -> Option<&Value> {
    bay_hooks(settings, "PreToolUse", "pre-tool").next()
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

/// Replaces bay's groups for every Claude event and leaves every other entry where it was. A
/// bay group under an event bay no longer hooks is taken out, and the event with it when
/// nothing else is left there.
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
    hooks.retain(|event, groups| {
        if CLAUDE_EVENTS.iter().any(|(hooked, _)| hooked == event) {
            return true;
        }
        let Some(list) = groups.as_array_mut() else {
            return true;
        };
        let before = list.len();
        list.retain(|group| !is_bay_group(group));
        !(list.is_empty() && before > 0)
    });
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
    crate::durable::rename(&tmp, path).map_err(|error| format!("{}: {error}", path.display()))
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

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::add_claude_entries;

    #[test]
    fn an_unprobed_event_loses_bays_entry_and_keeps_every_other() {
        // Ruling R16: an installed `WorktreeCreate` hook is behaviour (it replaces the product's
        // own worktree creation), so bay installs none until it is probed, and `install` takes
        // back an entry an earlier bay wrote. Another tool's entry stays byte for byte.
        let other = json!({"hooks": [{"type": "command", "command": "other-tool make-worktree"}]});
        let mut settings: Map<String, Value> = serde_json::from_value(json!({"hooks": {
            "WorktreeCreate": [
                {"hooks": [{"type": "command", "command": "bay hook worktree-create"}]},
                other.clone(),
            ],
            "CwdChanged": [{"hooks": [{"type": "command", "command": "bay hook cwd-changed"}]}],
        }}))
        .unwrap();
        add_claude_entries(&mut settings);
        let hooks = &settings["hooks"];
        assert_eq!(hooks["WorktreeCreate"], json!([other]));
        assert!(hooks.get("CwdChanged").is_none(), "{hooks}");
        assert!(hooks.get("WorktreeRemove").is_none(), "{hooks}");
        assert!(hooks.get("StopFailure").is_none(), "{hooks}");
        assert!(hooks.get("TeammateIdle").is_some(), "{hooks}");
    }
}

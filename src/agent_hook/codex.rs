//! Codex's payloads (`tests/fixtures/codex-0.153.2`, G308): the fields the shared event
//! handling reads, as the recordings show them (ruling R16).
//!
//! The recorded events are `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
//! `Stop`, `SubagentStart`, `SubagentStop` and `SessionEnd`; any other name has no behaviour.
//! The shell is `tool_name: "Bash"` with `tool_input.command`, the shape Claude Code's has
//! (fixture 05). `apply_patch` names no target field: its targets are the patch's file headers
//! in `tool_input.command` (fixture 03).

use std::path::PathBuf;

use serde_json::Value;

use super::Event;

/// Codex's edit tool (fixtures 03, 04).
const APPLY_PATCH: &str = "apply_patch";

/// The file headers of an `apply_patch` patch, each followed by the path it writes: the four
/// the Codex probe named (`Add File`, `Update File`, `Delete File`, `Move to`).
const PATCH_HEADERS: [&str; 4] = [
    "*** Add File: ",
    "*** Update File: ",
    "*** Delete File: ",
    "*** Move to: ",
];

/// The bay event of a recorded Codex event name; [`Event::Unprobed`] for any other.
pub(super) fn event(name: &str) -> Event {
    match name {
        "SessionStart" => Event::SessionStart,
        "UserPromptSubmit" => Event::UserPromptSubmit,
        "PreToolUse" => Event::PreToolUse,
        "PostToolUse" => Event::PostToolUse,
        "Stop" => Event::Stop,
        "SubagentStart" => Event::SubagentStart,
        "SubagentStop" => Event::SubagentStop,
        "SessionEnd" => Event::SessionEnd,
        _ => Event::Unprobed,
    }
}

/// The targets an `apply_patch` call names, as its headers give them: absolute, or relative to
/// `cwd`; none when its patch has no header. `None` when `tool` is not `apply_patch`.
///
/// # Errors
///
/// An `apply_patch` call with no string `tool_input.command`: bay cannot tell what it writes.
pub(super) fn edit_targets(
    tool: &str,
    value: &Value,
) -> Result<Option<Vec<PathBuf>>, &'static str> {
    if tool != APPLY_PATCH {
        return Ok(None);
    }
    let patch = value["tool_input"]["command"]
        .as_str()
        .ok_or("is an apply_patch call with no string tool_input.command")?;
    Ok(Some(patch_targets(patch)))
}

/// The paths of `patch`'s file headers, in patch order.
fn patch_targets(patch: &str) -> Vec<PathBuf> {
    patch
        .lines()
        .filter_map(|line| {
            PATCH_HEADERS
                .iter()
                .find_map(|header| line.strip_prefix(header))
        })
        .map(|path| path.trim_end_matches('\r'))
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// The `apply_patch` targets a `PostToolUse` payload names (fixture 04); none for another event
/// or tool. The shell names none (fixture 06): the view's Git status is the write record.
pub(super) fn edited(event: Event, value: &Value) -> Vec<PathBuf> {
    if event != Event::PostToolUse {
        return Vec::new();
    }
    value["tool_name"]
        .as_str()
        .and_then(|tool| edit_targets(tool, value).ok().flatten())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::patch_targets;

    #[test]
    fn a_patch_names_each_file_its_headers_write() {
        let patch = "*** Begin Patch\n*** Add File: /r/a.txt\n+a\n*** Update File: b.txt\n\
                     *** Move to: c.txt\n@@\n-x\n+y\n*** Delete File: d.txt\n*** End Patch";
        assert_eq!(
            patch_targets(patch),
            ["/r/a.txt", "b.txt", "c.txt", "d.txt"]
                .map(PathBuf::from)
                .to_vec()
        );
        assert_eq!(patch_targets("*** Begin Patch\n*** End Patch").len(), 0);
    }
}

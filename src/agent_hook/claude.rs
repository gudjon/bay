//! Claude Code's payloads (`tests/fixtures/claude-code-2.1.302`): the fields the shared event
//! handling reads, as the recordings show them (ruling R16).
//!
//! An edit tool's target is `tool_input.file_path`, the field fixture 03 shows. A post-tool
//! step's edit targets are in a `PostToolBatch`'s `tool_calls` (fixture 08), or in a
//! `PostToolUse`'s own call (fixture 04).

use std::path::PathBuf;

use serde_json::Value;

use super::Event;

/// The edit tools whose target `pre-tool` judges (§ 11a's `PreToolUse` row).
pub(super) const EDIT_TOOLS: [&str; 3] = ["Edit", "Write", "NotebookEdit"];

/// The targets `tool` names when it is an edit tool, as the payload gives them: absolute, or
/// relative to `cwd`; none when it names none (a `NotebookEdit` names its target in a field no
/// probe recorded, R16). `None` when `tool` is no edit tool.
pub(super) fn edit_targets(tool: &str, value: &Value) -> Option<Vec<PathBuf>> {
    EDIT_TOOLS.contains(&tool).then(|| {
        value["tool_input"]["file_path"]
            .as_str()
            .map(PathBuf::from)
            .into_iter()
            .collect()
    })
}

/// The edit-tool targets a post-tool payload names; none for another event.
pub(super) fn edited(event: Event, value: &Value) -> Vec<PathBuf> {
    let calls: Vec<&Value> = match event {
        Event::PostToolBatch => value["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .collect(),
        Event::PostToolUse => vec![value],
        _ => Vec::new(),
    };
    calls
        .into_iter()
        .filter_map(|call| edit_targets(call["tool_name"].as_str()?, call))
        .flatten()
        .collect()
}

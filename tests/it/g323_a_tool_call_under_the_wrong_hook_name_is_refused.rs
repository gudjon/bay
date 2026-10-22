//! G323 (found by the hooks research, reproduced on 0.2.16): a tool call that reaches `bay hook`
//! under the wrong name is refused, never let through.
//!
//! `bay hook <event>` takes the moment from its command line. A payload that carries a tool
//! call, under a name bay does not know (a typo, or a settings file and a bay from different
//! versions), or a `PreToolUse` payload under any name but `tool.before`, is denied with
//! `HOOK_INPUT_INVALID` (exit 2) and a work-record row. Before this, such a call ran with exit 0
//! and no row, so a misnamed entry with `onFailure: "block"` let every write through silently.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/agent_hook.rs` `dispatch_as`: drop the `misnamed_tool_call` refusal (the force push
//!   under an unknown name exits 0).
//! - `src/agent_hook.rs` `misnamed_tool_call`: `if !known {` to `if false {` (an unknown name
//!   exits 0).
//! - `src/agent_hook.rs` `misnamed_tool_call`: `== "PreToolUse"` to `== "Nothing"` (a
//!   `PreToolUse` payload under `step.end` exits 0).
//! - `src/agent_hook.rs` `dispatch_as`: `invalid_payload(.., true)` to `false` (the refusal
//!   says to update bay, not to rewrite the entry).

// Each case runs `bay hook` as the product does, with the payload on stdin; Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::TestBox;

/// `bay hook <event>` in `cwd` with `payload` on stdin: its exit code and stderr.
fn hook(test_box: &TestBox, cwd: &Path, event: &str, payload: &Value) -> (i32, String) {
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn force_push(cwd: &Path) -> Value {
    json!({
        "hook_event_name": "PreToolUse",
        "session_id": "s-323",
        "cwd": cwd.display().to_string(),
        "tool_name": "Bash",
        "tool_input": {"command": "git push --force origin main"},
    })
}

#[test]
fn a_tool_call_under_an_unknown_or_wrong_name_is_refused() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let payload = force_push(&repo);

    // The right name: the deny list refuses it, as always.
    let (code, stderr) = hook(&test_box, &repo, "tool.before", &payload);
    assert_eq!(code, 2, "{stderr}");

    // An unknown name, and a known name that is not pre-tool: refused, never let through.
    for event in ["pre-tool", "pretool", "step.end"] {
        let (code, stderr) = hook(&test_box, &repo, event, &payload);
        assert_eq!(
            code, 2,
            "bay hook {event} let a tool call through: {stderr}"
        );
        assert!(stderr.contains(event), "{event}: {stderr}");
        // The step is the hook entry, not a newer bay.
        assert!(stderr.contains("`bay install`"), "{event}: {stderr}");
    }

    // An unknown name is refused for any tool call, even one whose payload names no event.
    let mut unnamed = payload.clone();
    unnamed.as_object_mut().unwrap().remove("hook_event_name");
    let (code, stderr) = hook(&test_box, &repo, "pre-tool", &unnamed);
    assert_eq!(code, 2, "an unknown name let a tool call through: {stderr}");

    // A known name whose payload carries no tool call still runs as before.
    let idle = json!({
        "hook_event_name": "Notification",
        "session_id": "s-323",
        "cwd": repo.display().to_string(),
    });
    let (code, stderr) = hook(&test_box, &repo, "session.wait", &idle);
    assert_eq!(code, 0, "{stderr}");
}

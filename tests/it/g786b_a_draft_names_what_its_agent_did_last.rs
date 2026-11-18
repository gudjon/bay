//! G786b (owner, 2026-11-18: a draft is estimated by what it is doing, and flagged when idle;
//! a fleet finding the same day: agents work in the main checkout as often as in a view, so the
//! hook's lines name no change). Each step's line in the session's work record names what the
//! step did (`activity`: a check the policy on main declares, a commit, a bay command, another
//! command, an edit, a read), and a change's row names what the session that holds it did last
//! (`last_activity`: when, what, and whether it was in the change's own view), its turn's end
//! (`idle`) included.
//!
//! Guards to mutate: in `src/session.rs` `payload_fields`, drop the `activity` field (a step
//! says only that the session works); in `src/status.rs` `live_fields`, set `last_activity` to
//! null.

#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::{TestBox, check_files, passing_result, result_json};

const SESSION: &str = "s-work-1";

/// Sends `bay hook step.end` the `event` payload of session [`SESSION`] working in `dir`, with
/// `calls` as the batch's tool calls.
fn hook(test_box: &TestBox, dir: &Path, event: &str, calls: &[Value]) {
    let payload = json!({
        "hook_event_name": event,
        "session_id": SESSION,
        "cwd": dir.display().to_string(),
        "tool_calls": calls,
    });
    let mut child = test_box
        .bay_process(dir)
        .args(["hook", "step.end"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

fn shell(command: &str) -> Value {
    json!({"tool_name": "Bash", "tool_input": {"command": command}, "tool_response": ""})
}

/// The `activity` of the session's last line in `repo`'s store.
fn last_line_activity(repo: &Path) -> Value {
    let file = repo
        .join(".git/bay/sessions")
        .join(format!("{SESSION}.jsonl"));
    let text = std::fs::read_to_string(&file).unwrap();
    let line: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    line["activity"].clone()
}

#[test]
fn a_draft_names_what_its_agent_did_last() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (code, started) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_SESSION", SESSION)
            .args(["start", "CARD-1", "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{started}");
    let change = started["change"].as_str().unwrap().to_owned();

    // WHEN the session that holds it runs the declared check from the main checkout
    hook(
        &test_box,
        &repo,
        "PostToolBatch",
        &[shell("cd . && ./scripts/bay-check --fast")],
    );

    // THEN its line says a check ran, and the change says it is what its agent did last
    assert_eq!(last_line_activity(&repo), "check");
    let read = test_box.bay_ok(&repo, &["status", &change]);
    let last = &read["detail"]["changes"][0]["last_activity"];
    assert_eq!(last["kind"], "check", "{read}");
    assert_eq!(last["in_view"], false, "{read}");
    assert!(last["at"].as_str().is_some(), "{read}");

    // AND each other kind of step is named for what it did
    for (calls, kind) in [
        (vec![shell("git commit -m wip")], "commit"),
        (vec![shell("bay status")], "bay"),
        (vec![shell("ls -la")], "command"),
        (
            vec![json!({"tool_name": "Read", "tool_input": {"file_path": "src/a.rs"}})],
            "read",
        ),
    ] {
        hook(&test_box, &repo, "PostToolBatch", &calls);
        assert_eq!(last_line_activity(&repo), kind, "{calls:?}");
    }

    // AND when its turn ends, the change says its agent is idle
    hook(&test_box, &repo, "Stop", &[]);
    let read = test_box.bay_ok(&repo, &["status", &change]);
    assert_eq!(
        read["detail"]["changes"][0]["last_activity"]["kind"], "idle",
        "{read}"
    );
}

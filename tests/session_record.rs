//! The per-session work record (`DESIGN` § 11a, `INTERFACES` § 5a) and ruling 10.
//!
//! A view belongs to one change: every session whose hook fires in a view works on that
//! view's change and is recorded by its own session id under the change's actor. That holds for
//! a subagent (the parent's `session_id` with its own `agent_id`) and for a teammate session
//! (its own `session_id`, no `SubagentStart`) writing into the lead's view.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};
use support::TestBox;

fn hook(test_box: &TestBox, cwd: &Path, event: &str, payload: &Value) -> Option<i32> {
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
    child.wait_with_output().unwrap().status.code()
}

fn lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn sessions_dir(repo: &Path) -> PathBuf {
    repo.join(".git/bay/sessions")
}

#[test]
fn every_session_in_a_view_is_recorded_on_the_views_change() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let (id, view) = test_box.start_and_edit(&repo);
    let record: Value = serde_json::from_str(&test_box.git_ok(
        &repo,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    ))
    .unwrap();
    let actor = &record["actor"];

    // The lead, a subagent of the lead, and a teammate with its own session.
    let lead = json!({"session_id": "lead-1", "hook_event_name": "SessionStart", "source": "startup", "cwd": view});
    assert_eq!(hook(&test_box, &view, "session-start", &lead), Some(0));
    let batch = json!({"session_id": "lead-1", "hook_event_name": "PostToolBatch", "cwd": view, "tool_calls": []});
    assert_eq!(hook(&test_box, &view, "post-tool", &batch), Some(0));
    let sub = json!({"session_id": "lead-1", "agent_id": "a-sub", "agent_type": "general-purpose",
        "hook_event_name": "PostToolBatch", "cwd": view, "tool_calls": []});
    assert_eq!(hook(&test_box, &view, "post-tool", &sub), Some(0));
    let mate = json!({"session_id": "mate-2", "agent_type": "teammate", "hook_event_name": "SessionStart",
        "source": "startup", "cwd": view});
    assert_eq!(hook(&test_box, &view, "session-start", &mate), Some(0));
    let mate_batch = json!({"session_id": "mate-2", "hook_event_name": "PostToolBatch", "cwd": view, "tool_calls": []});
    assert_eq!(hook(&test_box, &view, "post-tool", &mate_batch), Some(0));

    let lead_lines = lines(&sessions_dir(&repo).join("lead-1.jsonl"));
    let mate_lines = lines(&sessions_dir(&repo).join("mate-2.jsonl"));
    assert_eq!(lead_lines.len(), 3, "{lead_lines:?}");
    assert_eq!(mate_lines.len(), 2, "{mate_lines:?}");
    for (session, line) in lead_lines
        .iter()
        .map(|line| ("lead-1", line))
        .chain(mate_lines.iter().map(|line| ("mate-2", line)))
    {
        assert_eq!(line["schema"], "bay.session/1", "{line}");
        assert_eq!(line["session"], session, "{line}");
        assert_eq!(line["change"], id.as_str(), "{line}");
        assert_eq!(&line["actor"], actor, "the change's actor: {line}");
        assert!(line["at"].is_string(), "{line}");
    }
    assert_eq!(lead_lines[0]["event"], "session_start");
    assert_eq!(lead_lines[1]["event"], "post_tool_batch");
    assert_eq!(lead_lines[1]["state"], "working");
    assert!(lead_lines[1].get("agent").is_none(), "{}", lead_lines[1]);
    assert_eq!(
        lead_lines[2]["agent"], "a-sub",
        "a subagent keeps its agent id"
    );
}

#[test]
fn a_session_outside_a_view_is_recorded_with_no_change() {
    // § 11a: every hook event appends a line, and only a session with hooks disabled is
    // untracked. A session in the repository but in no view works on no change.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let payload = json!({"session_id": "root-1", "hook_event_name": "SessionStart", "cwd": repo});
    assert_eq!(hook(&test_box, &repo, "session-start", &payload), Some(0));
    let recorded = lines(&sessions_dir(&repo).join("root-1.jsonl"));
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    let line = &recorded[0];
    assert_eq!(line["schema"], "bay.session/1", "{line}");
    assert_eq!(line["event"], "session_start", "{line}");
    assert!(line["change"].is_null(), "{line}");
    assert!(line["view"].is_null(), "{line}");
    assert_eq!(line["actor"]["name"], "agent", "{line}");
    assert_eq!(line["actor"]["session"], "root-1", "{line}");
}

#[test]
fn pre_tool_appends_nothing() {
    // PreToolUse is the blocking path (`onFailure: "block"`): § 11a gives it only the refusal,
    // and the step's PostToolBatch records the step.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let (_, view) = test_box.start_and_edit(&repo);
    let payload = json!({"session_id": "pre-1", "hook_event_name": "PreToolUse", "cwd": view,
        "tool_name": "Edit", "tool_input": {"file_path": view.join("src/a.rs")}});
    assert_eq!(hook(&test_box, &view, "pre-tool", &payload), Some(0));
    assert!(!sessions_dir(&repo).join("pre-1.jsonl").exists());
}

#[test]
fn a_session_id_that_is_not_a_file_name_is_not_recorded() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let (_, view) = test_box.start_and_edit(&repo);
    let payload = json!({"session_id": "../escape", "hook_event_name": "Stop", "cwd": view});
    assert_eq!(hook(&test_box, &view, "stop", &payload), Some(0));
    assert!(!repo.join(".git/bay/escape.jsonl").exists());
    assert!(!sessions_dir(&repo).exists(), "nothing was recorded");
}

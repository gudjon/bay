//! G707 (a measurement, 2026-11-12: the view's `git status` took 1.4 s on a 52,000-file tree at
//! every tool batch). A batch whose tools write no file (`Read`, `Grep`, `Glob` and the like)
//! leaves the view as it was, so its hook reads no status; the next batch's read finds whatever
//! came before it. A batch with a tool that may write still reads it.
//!
//! Read from the trace a test build writes when `BAY_TEST_GIT_TRACE` names a file (G171).
//!
//! Guard to mutate: in `src/agent_hook.rs` `Payload::writes`, the `reads_only` test (every batch
//! reads the status).

#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};
use support::{TestBox, unchecked_files};

/// The `status` calls of `bay hook step.end` in `view` after a batch of `tools`.
fn statuses(test_box: &TestBox, view: &Path, tools: &[&str], name: &str) -> usize {
    let trace = test_box.path().join(format!("{name}.jsonl"));
    let calls: Vec<Value> = tools
        .iter()
        .map(|tool| json!({"tool_name": tool, "tool_input": {}, "tool_response": ""}))
        .collect();
    let payload = json!({
        "hook_event_name": "PostToolBatch",
        "session_id": "s-1",
        "cwd": view.display().to_string(),
        "tool_calls": calls,
    });
    let mut child = test_box
        .bay_process(view)
        .env("BAY_TEST_GIT_TRACE", &trace)
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
    std::fs::read_to_string(&trace)
        .unwrap_or_default()
        .lines()
        .filter(|line| serde_json::from_str::<Value>(line).unwrap()["sub"] == "status")
        .count()
}

#[test]
fn a_tool_batch_that_writes_nothing_reads_no_status() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let started = test_box.bay_ok(&repo, &["start", "READS", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    // GIVEN a session working in its change's view, its first step read once
    let _ = statuses(&test_box, &view, &["Write"], "first");

    // WHEN a batch only reads THEN its hook reads no status
    assert_eq!(statuses(&test_box, &view, &["Read", "Grep"], "reads"), 0);

    // AND a batch that may write still reads it
    assert!(statuses(&test_box, &view, &["Read", "Bash"], "writes") > 0);
}

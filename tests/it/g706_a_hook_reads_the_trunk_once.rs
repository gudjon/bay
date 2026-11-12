//! G706 (a measurement, 2026-11-12: the hook before each tool call ran 15 Git calls, 9 of them the
//! same reads of the remote's default branch and the trunk's tip). A hook process keeps each ref
//! read until it runs a Git command that may move a ref: before a tool call it reads the
//! remote's default branch and the trunk's tip once each.
//!
//! Read from the trace a test build writes when `BAY_TEST_GIT_TRACE` names a file (G171).
//!
//! Guard to mutate: in `src/agent_hook.rs` `dispatch`, the `keep_ref_reads` call (each read runs
//! again).

#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::process::Stdio;

use serde_json::{Value, json};
use support::{TestBox, unchecked_files};

#[test]
fn a_hook_reads_the_trunk_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let trace = test_box.path().join("trace.jsonl");
    // GIVEN an agent's tool call in the repository WHEN its hook runs
    let payload = json!({
        "hook_event_name": "PreToolUse",
        "session_id": "s-1",
        "tool_name": "Bash",
        "tool_input": {"command": "ls src"},
        "cwd": repo.display().to_string(),
    });
    let mut child = test_box
        .bay_process(&repo)
        .env("BAY_TEST_GIT_TRACE", &trace)
        .args(["hook", "tool.before"])
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

    // THEN it read the remote's default branch and the trunk's tip once each
    let calls: Vec<Vec<String>> = std::fs::read_to_string(&trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .map(|line| {
            line["args"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .collect();
    let count = |sub: &str, last: &str| {
        calls
            .iter()
            .filter(|args| {
                args.iter().any(|arg| arg == sub)
                    && args.last().is_some_and(|arg| arg.contains(last))
            })
            .count()
    };
    assert_eq!(
        count("symbolic-ref", "refs/remotes/origin/HEAD"),
        1,
        "{calls:?}"
    );
    assert_eq!(
        count("rev-parse", "refs/remotes/origin/main"),
        1,
        "{calls:?}"
    );
}

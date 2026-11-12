//! G700 (a fleet finding, 2026-11-12: a 17-minute check cost its runner some 3,400 `rev-list`
//! and 3,400 `cat-file` calls). Each heartbeat of a running check read every state of the
//! change's chain to find its newest draft, and each beat adds a state, so a long check's beats
//! cost its square. A beat reads the head state alone: the chain's states read per beat stay
//! constant however long the check runs.
//!
//! Read from the trace a test build writes when `BAY_TEST_GIT_TRACE` names a file (G171).
//!
//! Guard to mutate: in `src/publish.rs` `heartbeat`, `latest_draft` (back to `drafts`, a walk
//! of every state).

#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_heartbeat_costs_the_same_late_in_a_long_check() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("sleep 20", &passing_result(1), 1, 120);
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nlease_s = 30\nheartbeat_s = 1\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");

    // GIVEN a runner whose check runs for a while, beating its change's chain each second
    let executor = test_box.clone_of(&remote, "executor");
    let trace = test_box.path().join("trace.jsonl");
    let (code, ran) = result_json(
        test_box
            .bay(&executor)
            .args(["runner", "run", "--once"])
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{ran}");

    // WHEN its Git work is counted
    let lines: Vec<Value> = std::fs::read_to_string(&trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let args = |line: &Value| -> Vec<String> {
        line["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    };
    let beats = lines
        .iter()
        .filter(|line| {
            line["sub"] == "push"
                && args(line)
                    .iter()
                    .any(|arg| arg.contains("refs/bay/changes/"))
        })
        .count();
    let states_read = lines
        .iter()
        .filter(|line| line["sub"] == "rev-list" && args(line).iter().any(|arg| arg == "--parents"))
        .count();

    // THEN it read a few states per beat, not the whole chain each time
    assert!(beats >= 10, "only {beats} beats");
    assert!(
        states_read <= 2 * beats + 20,
        "{states_read} chain states read for {beats} beats"
    );
}

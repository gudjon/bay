//! G695 (a fleet finding, 2026-11-10: every runner pass listed 30,000 remote refs, 2.3 MB and
//! 1.7 s, to learn they were under a budget of 100,000). A pass's compaction counts the remote's
//! bay refs; once a count finds them under the budget, the passes of the next hour list none.
//! A count over the budget stands for nothing: the next pass counts again (G21's compaction).
//!
//! Read from the trace a test build writes when `BAY_TEST_GIT_TRACE` names a file (G171).
//!
//! Guard to mutate: in `src/compact.rs` `run`, the `counted` return (each pass lists the
//! remote's refs again).

#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

/// The listings of every bay ref one `bay runner run --once` in `dir` made.
fn listings(test_box: &TestBox, dir: &Path, name: &str) -> usize {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let (code, result) = result_json(
        test_box
            .bay(dir)
            .args(["runner", "run", "--once"])
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{result}");
    std::fs::read_to_string(&trace)
        .expect("the trace was written")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| line["sub"] == "ls-remote")
        .filter(|line| {
            line["args"]
                .as_array()
                .is_some_and(|args| args.iter().any(|arg| arg == "refs/bay/*"))
        })
        .count()
}

#[test]
fn a_pass_under_the_ref_budget_lists_the_remote_hourly() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let (_, view) = test_box.start_and_edit(&repo);
    test_box.bay_ok(&view, &["merge", "--wait"]);
    let executor = test_box.clone_of(&remote, "executor");

    // GIVEN a pass that counted the remote's bay refs under the budget
    assert_eq!(listings(&test_box, &executor, "first"), 1);

    // WHEN the next pass runs THEN it lists none
    assert_eq!(listings(&test_box, &executor, "second"), 0);
}

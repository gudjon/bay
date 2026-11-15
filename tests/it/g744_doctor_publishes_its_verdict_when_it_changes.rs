//! G744 (a measurement, 2026-11-15: a machine ran `bay doctor` about once a minute, each one
//! waiting on a push of the machine's summary, 12 s at the median, for a verdict that had not
//! changed). Doctor publishes its verdict on the machine's live summary (G314) when the verdict
//! changed, or when the one published is ten minutes old; a doctor whose verdict stands as
//! published pushes nothing. Read from the trace a test build writes (G171).
//!
//! Guard to mutate: in `src/live.rs` `publish_doctor`, never take the verdict out as standing, and
//! the second doctor pushes the summary again.

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::{TestBox, unchecked_files};

/// One `bay doctor` in `repo`: how many pushes its own process made.
fn doctor_pushes(test_box: &TestBox, repo: &Path, name: &str) -> usize {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let _ = test_box
        .bay(repo)
        .args(["doctor"])
        .env("BAY_TEST_GIT_TRACE", &trace)
        .output()
        .unwrap();
    let lines: Vec<Value> = std::fs::read_to_string(&trace)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref() && line["sub"] == "push")
        .filter(|line| line["args"].to_string().contains("refs/bay/live/"))
        .count()
}

#[test]
fn a_doctor_whose_verdict_stands_as_published_pushes_nothing() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    // GIVEN a session on the machine, so it has a live summary to publish
    test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);

    // WHEN doctor runs twice
    let first = doctor_pushes(&test_box, &repo, "first");
    let second = doctor_pushes(&test_box, &repo, "second");

    // THEN the first published the verdict and the second, which stands, pushed nothing
    assert_eq!(first, 1, "the first doctor did not publish its verdict");
    assert_eq!(second, 0, "the second doctor pushed a verdict that stands");
}

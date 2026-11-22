//! G856 (a fleet measurement, 2026-11-22: runners holding a 450 s lease pushed a lease beat each
//! 10 s heartbeat of a check, a median 40 s, at about 5 s a push to the host, while no reader
//! judges a holder quiet before 450 s). A holder beats its lease an eighth of the lease apart, and
//! never more often than once a heartbeat; a record that changed goes out at the next heartbeat.
//! A short lease (eight heartbeats or less) is still beaten each heartbeat
//! (`g787_a_runner_beats_its_lease_never_the_change_history`).
//!
//! The guard to mutate: `beat_every` in `src/publish.rs` returning `config.heartbeat()` (a beat
//! each heartbeat, as before G856), which makes about eight lease pushes in this test's check.

#![cfg(unix)]

use crate::support;

use std::path::Path;
use std::process::Stdio;

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// The pushes in the trace at `trace` that name a ref under `prefix`.
fn pushes(trace: &Path, prefix: &str) -> usize {
    std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| {
            line["sub"] == "push"
                && line["args"].as_array().is_some_and(|args| {
                    args.iter()
                        .any(|arg| arg.as_str().is_some_and(|arg| arg.contains(prefix)))
                })
        })
        .count()
}

#[test]
fn a_long_lease_is_beaten_an_eighth_of_it_apart_not_each_heartbeat() {
    // GIVEN a change whose check runs for eight seconds under a one-second heartbeat and an
    // 80 s lease: a beat is due every 10 s
    let check_s = 8;
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files(&format!("sleep {check_s}"), &passing_result(1), 1, 120);
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nlease_s = 80\nheartbeat_s = 1\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    let runner = test_box.clone_of(&remote, "exec-a");
    test_box.git_ok(&runner, &["config", "bay.runnerName", "exec-a"]);
    test_box.git_ok(&runner, &["config", "bay.producerName", "exec-a-checks"]);
    let trace = test_box.path().join("trace.jsonl");

    // WHEN a runner checks and lands it
    let out = test_box
        .bay_process(&runner)
        .args(["runner", "run", "--once", "--json"])
        .env("BAY_TEST_GIT_TRACE", &trace)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    let ran: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id], "{ran}");

    // THEN its lease went out at most twice (its first beat, and once more if the record named
    // its batch after that), plus the release: never once a heartbeat
    let lease_pushes = pushes(&trace, "refs/bay/lease/");
    eprintln!("G856 measure: a {check_s} s check under an 80 s lease: {lease_pushes} lease pushes");
    assert!(
        (1..=3).contains(&lease_pushes),
        "{lease_pushes} lease pushes in an {check_s} s check under an 80 s lease"
    );
}

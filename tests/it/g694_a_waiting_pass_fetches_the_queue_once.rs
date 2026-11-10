//! G694 (a fleet finding, 2026-11-10: a runner's pass fetched the queue four times). A pass reads
//! the queue first to release heads that ended or stalled, then shared its reads afresh, so its
//! first visit fetched the queue again a moment later. Now that read is the pass's shared read of
//! the queue: a pass whose visits only wait behind the head fetches the queue once.
//!
//! A visit that used a shared read and may run its operation reads again only what was read
//! before it began (`src/repo.rs` `read_again`), never what it fetched itself; G680's budget holds
//! a pass of three landings to it.
//!
//! Read from the trace a test build writes when `BAY_TEST_GIT_TRACE` names a file (G171).
//!
//! Guard to mutate: in `src/executor.rs` `pass`, the `read_at` of the queue the deadlines read
//! (the first visit fetches the queue again).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// The queue fetches of one `bay runner run --once` in `dir`, from bay's own process only, and
/// the pass's result.
fn queue_fetches(test_box: &TestBox, dir: &Path, name: &str) -> (Vec<String>, Value) {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let (code, result) = result_json(
        test_box
            .bay(dir)
            .args(["runner", "run", "--once"])
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{result}");
    let lines: Vec<Value> = std::fs::read_to_string(&trace)
        .expect("the trace was written")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let made = lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref() && line["sub"] == "fetch")
        .map(|line| {
            line["args"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|args| args.contains("refs/bay/queue/"))
        .collect();
    (made, result)
}

#[test]
fn a_waiting_pass_fetches_the_queue_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        // A lease long enough that the idle-head check (two leases) never takes the head that
        // holds the line, an operation that does not exist.
        "[runner]\nahead = 8\nlease_s = 60\nheartbeat_s = 1\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    // GIVEN a head that holds the line, and a change waiting behind it
    let (open, _) = test_box.start_and_edit(&repo);
    let handle = bay::repo::Repo::open(&repo).unwrap();
    bay::queue::join(&handle, None, &format!("{open}.1"), &open).unwrap();
    let started = test_box.bay_ok(&repo, &["start", "WAIT", "--paths", "src"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/w.rs"), "fn w() {}\n").unwrap();
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    let waiting = queued["change"].as_str().unwrap().to_owned();
    let executor = test_box.clone_of(&remote, "executor");
    let _ = queue_fetches(&test_box, &executor, "place");

    // WHEN a pass finds it waiting
    let (made, result) = queue_fetches(&test_box, &executor, "wait");
    assert!(
        result["detail"]["ran"].as_array().is_some_and(|ran| ran
            .iter()
            .any(|row| row["change"] == waiting.as_str() && row["decision"] == "queued")),
        "{waiting} was not visited waiting: {result}"
    );

    // THEN it fetched the queue once
    assert_eq!(made.len(), 1, "{}", made.join("\n"));
}

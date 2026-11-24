//! G883 (a fleet finding, 2026-11-24: a `bay merge --wait=3600` still watched six minutes after
//! a runner on another machine merged its change, and ended only at its own limit). A timed wait
//! whose change a runner claims and merges in its own clone reads the change's chain from the
//! remote once a heartbeat, and answers `merged` soon after the runner merges it, with no other
//! process fetching in the waiting clone.
//!
//! The wait's own run is held before its claim and ends there (`BAY_TEST_PAUSE_AT`), so the
//! runner takes the change.
//!
//! Guard to mutate: `src/publish.rs` `watch_until_done`, set `elsewhere` to `false`: the wait
//! reads only its own clone and ends `queued` at its limit.

#![cfg(unix)]

use crate::support;

use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, check_files, passing_result};

#[test]
fn a_timed_wait_hears_of_a_runners_merge_in_its_own_clone() {
    let test_box = TestBox::new();
    // GIVEN a repository with one check, and a runner's clone on another machine
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let check = check_files("", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &check);
    test_box.git_ok(&repo, &["config", "bay.machine", "dev-box"]);
    let runner = test_box.clone_of(&remote, "runner");
    test_box.git_ok(&runner, &["config", "bay.machine", "runner-box"]);
    test_box.git_ok(&runner, &["config", "bay.runnerName", "exec-r"]);
    let (id, view) = test_box.start_and_edit(&repo);

    // WHEN a timed wait starts, and its own run ends before it claims the change
    let held = test_box.path().join("never");
    let waiting = test_box
        .bay_process(&view)
        .args(["--json", "merge", "--wait=90"])
        .env("BAY_TEST_PAUSE_AT", "before-claim")
        .env("BAY_TEST_PAUSE_FILE", &held)
        .env("BAY_TEST_PAUSE_TIMEOUT_S", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let paused = test_box.path().join("never.paused");
    let began = Instant::now();
    while !paused.exists() {
        assert!(
            began.elapsed() < Duration::from_secs(60),
            "the own run never came to its claim"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // AND the runner claims the change and merges it in its own clone
    for _ in 0..3 {
        test_box.bay_ok(&runner, &["runner", "run", "--once"]);
        if test_box.trunk_change_ids(&remote).contains(&id) {
            break;
        }
    }
    assert!(
        test_box.trunk_change_ids(&remote).contains(&id),
        "the runner did not merge the change"
    );
    let merged = Instant::now();

    // THEN the wait answers merged within a few heartbeats, not at its limit
    let out = waiting.wait_with_output().unwrap();
    let result: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stdout)));
    assert_eq!(result["decision"], "merged", "{result}");
    assert!(
        merged.elapsed() < Duration::from_secs(45),
        "the wait answered {:?} after the runner merged: {result}",
        merged.elapsed()
    );
}

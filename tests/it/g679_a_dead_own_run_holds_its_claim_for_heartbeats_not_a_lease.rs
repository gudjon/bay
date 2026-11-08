//! G679 (a fleet finding, 2026-11-08: twice in one hour a change's own `bay merge` claimed its
//! operation, recorded its check and stopped; both runners watched the dead claim for the full
//! 450 s lease and ran nothing else). A claim held by a change's own run, on a clone that is no
//! runner (bay's default runner name), is taken over once its chain is quiet for six
//! heartbeats; a runner's claim keeps the full lease. The change lands once.
//!
//! The guard to mutate: `holder_lease` in `src/publish.rs` returning the full lease for every
//! holder (the runner then waits the 120 s lease, past the bound here).

#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_runner_takes_over_a_dead_own_run_within_heartbeats() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nlease_s = 120\nheartbeat_s = 1\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    // A runner of its own name and producer, on its own clone.
    let runner = test_box.clone_of(&remote, "runner");
    test_box.git_ok(&runner, &["config", "bay.runnerName", "runner-1"]);
    test_box.git_ok(&runner, &["config", "bay.producerName", "runner-1"]);

    // GIVEN a change whose own run (the agent's clone names no runner) claimed it, recorded its
    // check, and died.
    let (id, view) = test_box.start_and_edit(&repo);
    let killed = test_box
        .bay(&view)
        .env("BAY_TEST_KILL_AT", "after-persist")
        .args(["merge", "--wait"])
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), Some(137));
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let operation = &status["detail"]["changes"][0]["operation"];
    assert_eq!(operation["state"], "checked", "{status}");

    // WHEN a runner's pass meets the dead claim THEN it takes it over within six heartbeats,
    // not the 120 s lease, and the change lands once.
    let began = Instant::now();
    let (code, ran) = result_json(test_box.bay(&runner).args(["runner", "run", "--once"]));
    let took = began.elapsed();
    assert_eq!(code, 0, "{ran}");
    assert!(
        took < Duration::from_secs(60),
        "the takeover took {took:?}: {ran}"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id], "{ran}");
}

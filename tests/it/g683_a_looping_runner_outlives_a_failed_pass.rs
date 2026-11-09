//! G683 (a fleet finding, 2026-11-09: a looping runner exited on one "Connection reset by peer"
//! from a fetch, and the queue waited until someone started it again). A looping runner that
//! has made a pass says why a later pass failed and tries again at its next pass; the remote
//! back, it lands what waits.
//!
//! Guard to mutate: in `src/executor.rs` `ends`, return the failed pass's result whatever
//! `retries` says (the runner exits while the remote is away, and the second change waits).

#![cfg(unix)]

use crate::support;

use std::process::Stdio;
use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

fn wait_for(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_looping_runner_outlives_a_failed_pass() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nlease_s = 30\nheartbeat_s = 1\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let exec = test_box.clone_of(&remote, "executor");
    test_box.git_ok(&exec, &["config", "bay.runnerName", "exec-a"]);
    test_box.git_ok(&exec, &["config", "bay.producerName", "exec-a-checks"]);
    let stderr = test_box.path().join("runner.err");
    let mut runner = test_box
        .bay_process(&exec)
        .args(["runner", "run", "--interval-s", "1"])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();

    // GIVEN a looping runner that has landed a change
    let (first, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    wait_for("the first landing", Duration::from_secs(60), || {
        test_box.trunk_change_ids(&remote).contains(&first)
    });

    // WHEN its remote is away for a pass
    let away = test_box.path().join("repo-remote.away");
    std::fs::rename(&remote, &away).unwrap();
    wait_for("a failed pass", Duration::from_secs(30), || {
        std::fs::read_to_string(&stderr)
            .unwrap_or_default()
            .contains("this pass failed")
    });
    assert!(
        runner.try_wait().unwrap().is_none(),
        "the runner exited: {}",
        std::fs::read_to_string(&stderr).unwrap()
    );
    std::fs::rename(&away, &remote).unwrap();

    // THEN it keeps running, and lands the next change once the remote is back
    let (second, view) = test_box.start_and_edit(&repo);
    std::fs::write(view.join("src/second.rs"), "fn second() {}\n").unwrap();
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    wait_for("the second landing", Duration::from_secs(60), || {
        test_box.trunk_change_ids(&remote).contains(&second)
    });
    runner.kill().unwrap();
    let _ = runner.wait();
}

/// A runner whose repository is gone stops: no later pass can serve it, and one a killed test
/// left behind would otherwise retry in a removed directory for good. Guard to mutate: in
/// `src/executor.rs` `ends`, drop `|| repository_gone(dir)` (the runner never exits).
#[test]
fn a_looping_runner_whose_repository_is_gone_stops() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nlease_s = 30\nheartbeat_s = 1\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let exec = test_box.clone_of(&remote, "executor");
    test_box.git_ok(&exec, &["config", "bay.runnerName", "exec-a"]);
    test_box.git_ok(&exec, &["config", "bay.producerName", "exec-a-checks"]);
    let mut runner = test_box
        .bay_process(&exec)
        .args(["runner", "run", "--interval-s", "1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // GIVEN a looping runner that has made passes
    let (first, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    wait_for("a landing", Duration::from_secs(60), || {
        test_box.trunk_change_ids(&remote).contains(&first)
    });
    // WHEN its repository is removed THEN it exits
    // The runner writes in it as it runs, so a removal may race a write: tried until it holds.
    wait_for("the removal", Duration::from_secs(10), || {
        std::fs::remove_dir_all(&exec).is_ok()
    });
    wait_for("the runner to exit", Duration::from_secs(30), || {
        runner.try_wait().unwrap().is_some()
    });
}

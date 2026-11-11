//! G696 (a fleet finding, 2026-11-11: every runner but the head's holder spent 20 to 35 s of
//! each pass, and up to 30 fetches, watching the holder's chain until its next heartbeat). A
//! looping runner that visits an operation another runner holds notes when it first saw the
//! holder's chain and judges the lease against that on its later passes; its pass never waits
//! for the holder to write. A `--once` pass, which has no later pass, still watches the lease.
//!
//! Guard to mutate: in `src/executor.rs` `run`, the `watch_leases_across_passes` call (each pass
//! of the looping runner waits inside its visit until the holder writes).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

/// An executor clone of `remote` named `name`.
fn executor(test_box: &TestBox, remote: &Path, name: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    clone
}

#[test]
fn a_looping_runner_never_waits_on_another_runners_lease() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    // One check that runs for a while; the holder writes nothing during the watch below.
    let files: Vec<(String, String)> =
        check_files(&test_box.relay("sleep 40"), &passing_result(1), 1, 120)
            .into_iter()
            .chain([(
                ".bay/config.toml".to_owned(),
                "[runner]\nlease_s = 120\nheartbeat_s = 60\n".to_owned(),
            )])
            .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");

    // GIVEN a runner that claimed the operation and runs its check
    let holder = executor(&test_box, &remote, "exec-a");
    let mut held = test_box
        .bay_process(&holder)
        .args(["runner", "run", "--once"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let chain = format!("refs/bay/changes/{id}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !test_box
        .git_bare(&remote, &["log", "--format=%s", &chain])
        .contains("by exec-a")
    {
        assert!(Instant::now() < deadline, "exec-a never claimed it");
        std::thread::sleep(Duration::from_millis(100));
    }

    // WHEN another runner loops for a while
    let watcher = executor(&test_box, &remote, "exec-b");
    let journal = test_box.path().join("exec-b.log");
    let mut looping = test_box
        .bay_process(&watcher)
        .args(["runner", "run", "--interval-s", "1"])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&journal).unwrap())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(12));
    let _ = looping.kill();
    let _ = looping.wait();
    let _ = held.kill();
    let _ = held.wait();

    // THEN it visited the held operation on pass after pass, never waiting inside one
    let text = std::fs::read_to_string(&journal).unwrap();
    let visits = text
        .lines()
        .filter(|line| line.starts_with(&format!("bay runner: {id}.1 ")))
        .count();
    assert!(visits >= 2, "{visits} visits in 12 s:\n{text}");
}

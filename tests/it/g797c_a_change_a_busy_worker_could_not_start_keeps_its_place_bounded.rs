//! G797 (owner ruling 2026-11-19, "keep place, bounded"; a fleet finding the same day: on a
//! shared test box a check not started within 120 s was answered `unavailable`, and its change
//! went to the back of the line). When a runner's worker answers `unavailable`, the change keeps
//! its place in line while another live runner serves its environment, and that runner takes it
//! there at once. After two hand-offs between runners, or one lease of waiting, it is released to
//! the back as before, so a busy box never holds the line.
//!
//! Guards to mutate, in `src/publish.rs` `keeps_place`: answer `None` (a free runner finds the
//! change at the back of the line, its place released); drop the `HANDOFFS` bound (two busy
//! runners pass it back and forth, and it holds its place past two hand-offs).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// A runner's clone of `remote` named `name`, which declares the environment `local`.
fn runner(test_box: &TestBox, remote: &Path, name: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    test_box.git_ok(&clone, &["config", "bay.checkEnvironment", "local"]);
    clone
}

/// A trunk whose one check finds its worker busy (answers `unavailable`) on its first `busy`
/// runs, and passes after. Returns the box, the work repository and the remote.
fn line(busy: u32) -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    let count = test_box.relay(&format!(
        "printf x >> '{runs}'; wc -c < '{runs}' | tr -d ' '",
        runs = runs.display()
    ));
    let before = format!(
        "n=$({count})\nif [ \"$n\" -le {busy} ]; then printf '%s\\n' '{{\"schema\":\"bay.check.result/1\",\"unavailable\":\"the worker did not start the check within 120 s\"}}'; exit 0; fi"
    );
    let files: Vec<(String, String)> = check_files(&before, &passing_result(1), 1, 60)
        .into_iter()
        .chain([(
            ".bay/config.toml".to_owned(),
            "[runner]\nlease_s = 300\nheartbeat_s = 1\n".to_owned(),
        )])
        .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (test_box, repo, remote)
}

/// Submits a change on `src/` named `intent` for a runner, behind every place in line. Returns
/// its change id and its place.
fn queue(test_box: &TestBox, repo: &Path, intent: &str) -> (String, u64) {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(format!("src/{intent}.rs")), "fn x() {}\n").unwrap();
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    let id = queued["change"].as_str().unwrap().to_owned();
    let op = queued["op_id"].as_str().unwrap();
    let handle = bay::repo::Repo::open(repo).unwrap();
    bay::queue::join(&handle, None, op, &id).unwrap();
    let place = bay::queue::Queue::read(&handle)
        .unwrap()
        .of(op)
        .unwrap()
        .seq;
    (id, place)
}

/// The reason the place `seq` was released with on the host, if it was.
fn released(test_box: &TestBox, remote: &Path, seq: u64) -> Option<String> {
    let name = format!("refs/bay/queue-done/{seq}");
    let listed = test_box.git_bare(remote, &["for-each-ref", "--format=%(refname)", &name]);
    if listed.trim().is_empty() {
        return None;
    }
    let release = test_box.git_bare(remote, &["show", &format!("{name}:release.json")]);
    let release: Value = serde_json::from_str(&release).unwrap();
    release["reason"].as_str().map(str::to_owned)
}

/// One `bay runner run --once` in `clone`, whose presence names a cadence of five minutes, so
/// it reads as live to the other runner however slowly this box runs the test.
fn pass(test_box: &TestBox, clone: &Path) -> Value {
    test_box.bay_ok(clone, &["runner", "run", "--once", "--interval-s", "300"])
}

#[test]
fn a_free_serving_runner_takes_a_busy_workers_change_in_its_place() {
    let (test_box, repo, remote) = line(1);
    let a = runner(&test_box, &remote, "exec-a");
    let b = runner(&test_box, &remote, "exec-b");
    // GIVEN runner B is live
    let _ = pass(&test_box, &b);
    let (change, place) = queue(&test_box, &repo, "first");

    // WHEN runner A's worker cannot start the check
    let busy = pass(&test_box, &a);
    assert_eq!(
        busy["detail"]["ran"][0]["reason"], "CHECK_UNAVAILABLE",
        "{busy}"
    );

    // THEN the change keeps its place
    assert_eq!(released(&test_box, &remote, place), None, "{busy}");

    // AND runner B takes it there and lands it
    let taken = pass(&test_box, &b);
    assert!(
        test_box.trunk_change_ids(&remote).contains(&change),
        "{taken}"
    );
    assert_eq!(
        released(&test_box, &remote, place).as_deref(),
        Some("published"),
        "{taken}"
    );
}

#[test]
fn two_busy_runners_hand_a_change_on_at_most_twice() {
    let (test_box, repo, remote) = line(3);
    let a = runner(&test_box, &remote, "exec-a");
    let b = runner(&test_box, &remote, "exec-b");
    let _ = pass(&test_box, &b);
    let (change, place) = queue(&test_box, &repo, "first");

    // GIVEN both runners' workers are busy: A hands the change to B, and B back to A
    let first = pass(&test_box, &a);
    assert_eq!(released(&test_box, &remote, place), None, "{first}");
    let second = pass(&test_box, &b);
    assert_eq!(released(&test_box, &remote, place), None, "{second}");

    // WHEN A's worker is busy once more
    let third = pass(&test_box, &a);

    // THEN the place goes, as a busy box never holds the line
    assert_eq!(
        released(&test_box, &remote, place).as_deref(),
        Some("unavailable"),
        "{third}"
    );
    assert!(
        !test_box.trunk_change_ids(&remote).contains(&change),
        "{third}"
    );
}

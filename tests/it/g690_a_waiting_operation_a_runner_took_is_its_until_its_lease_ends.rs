//! G690 (a fleet finding, 2026-11-09: one change was claimed 19 times in eight minutes by four
//! runners and never merged). An operation its holder left waiting for a worker holds no lease
//! against a box that serves it (G425), so that box takes it at once. The claim that takes it
//! ends the wait: the new holder runs it under a lease like any other. Before, the wait stayed
//! on the record, so every other serving runner took it from the one running its check, each
//! claim raised the epoch, and no check's result was ever kept.
//!
//! Guard to mutate: in `src/publish.rs` `claim`, the `op.unavailable = None` the claim writes
//! (the second serving runner takes the operation from the first while its check runs).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

/// An executor clone of `remote` named `name`, whose clone declares `environment`.
fn executor(test_box: &TestBox, remote: &Path, name: &str, environment: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    test_box.git_ok(&clone, &["config", "bay.checkEnvironment", environment]);
    clone
}

/// The claims written on change `id`'s chain at the remote, oldest first.
fn claims(test_box: &TestBox, remote: &Path, id: &str) -> Vec<String> {
    test_box
        .git_bare(
            remote,
            &[
                "log",
                "--reverse",
                "--format=%s",
                &format!("refs/bay/changes/{id}"),
            ],
        )
        .lines()
        .filter(|line| line.starts_with("claim "))
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_waiting_operation_a_runner_took_is_its_until_its_lease_ends() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    // One required check, in env-b, that takes a few seconds and counts its runs.
    let files: Vec<(String, String)> = check_files(
        &test_box.relay(&format!("sleep 4; echo \"$tree\" >> '{}'", runs.display())),
        &passing_result(1),
        1,
        60,
    )
    .into_iter()
    .map(|(path, text)| (path, text.replace("\"local\"", "\"env-b\"")))
    .chain([(
        ".bay/config.toml".to_owned(),
        "[runner]\nlease_s = 60\nheartbeat_s = 1\n".to_owned(),
    )])
    .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");

    // GIVEN an operation a box that cannot serve it left waiting for a worker
    let e1 = executor(&test_box, &remote, "exec-a", "env-a");
    let first = test_box.bay_ok(&e1, &["runner", "run", "--once"]);
    assert_eq!(
        first["detail"]["ran"][0]["reason"], "CHECK_UNAVAILABLE",
        "{first}"
    );
    // AND a serving box that took it and is running its check
    let e2 = executor(&test_box, &remote, "exec-b", "env-b");
    let e3 = executor(&test_box, &remote, "exec-c", "env-b");
    let mut taker = test_box
        .bay_process(&e2)
        .args(["runner", "run", "--once"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !claims(&test_box, &remote, &id)
        .iter()
        .any(|claim| claim.contains("by exec-b"))
    {
        assert!(Instant::now() < deadline, "exec-b never claimed it");
        std::thread::sleep(Duration::from_millis(100));
    }

    // WHEN another serving box makes a pass
    let _ = test_box.bay(&e3).args(["runner", "run", "--once"]).output();

    // THEN it leaves the operation to its holder, whose check runs once and merges
    assert!(taker.wait().unwrap().success());
    let claimed = claims(&test_box, &remote, &id);
    assert!(
        !claimed.iter().any(|claim| claim.contains("by exec-c")),
        "exec-c took the operation from exec-b while its check ran: {claimed:?}"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
    assert_eq!(
        std::fs::read_to_string(&runs).unwrap().lines().count(),
        1,
        "the check ran once"
    );
}

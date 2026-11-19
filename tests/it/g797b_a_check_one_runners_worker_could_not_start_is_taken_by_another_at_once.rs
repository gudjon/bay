//! G797 (a fleet finding, 2026-11-19: on a shared test box a check not started within 120 s was
//! answered `unavailable`, and the change then waited out its rejoin gap on every runner). An
//! adapter's `unavailable` answer is about the worker of the runner that asked: that runner
//! waits its gap before it tries the operation again, and another runner that serves the
//! environment takes it at once.
//!
//! Guard to mutate, in `src/publish.rs` `waits_for_worker`: the gap for every runner (drop
//! `answered_here`: the other runner passes the operation over for the whole gap).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

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

/// The claims on change `id`'s chain at the remote, oldest first.
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
fn a_check_one_runners_worker_could_not_start_is_taken_by_another_at_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    // The check's first run finds its worker busy: the adapter answers `unavailable`.
    let runs = test_box.path().join("runs");
    let count = test_box.relay(&format!(
        "printf x >> '{runs}'; wc -c < '{runs}' | tr -d ' '",
        runs = runs.display()
    ));
    let before = format!(
        "n=$({count})\nif [ \"$n\" -eq 1 ]; then printf '%s\\n' '{{\"schema\":\"bay.check.result/1\",\"unavailable\":\"the worker did not start the check within 120 s\"}}'; exit 0; fi"
    );
    let files: Vec<(String, String)> = check_files(&before, &passing_result(1), 1, 60)
        .into_iter()
        .chain([(
            ".bay/config.toml".to_owned(),
            // A rejoin gap of five minutes: no runner may wait it out within the test.
            "[runner]\nlease_s = 300\nheartbeat_s = 1\n".to_owned(),
        )])
        .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    let a = runner(&test_box, &remote, "exec-a");
    let b = runner(&test_box, &remote, "exec-b");

    // GIVEN runner A's worker could not start the check
    let first = test_box.bay_ok(&a, &["runner", "run", "--once"]);
    assert_eq!(
        first["detail"]["ran"][0]["reason"], "CHECK_UNAVAILABLE",
        "{first}"
    );
    // AND runner A waits its gap before it asks its worker again
    let again = test_box.bay_ok(&a, &["runner", "run", "--once"]);
    assert_eq!(claims(&test_box, &remote, &id).len(), 1, "{again}");

    // WHEN runner B, which serves the environment, passes
    let pass = test_box.bay_ok(&b, &["runner", "run", "--once"]);

    // THEN it takes the change at once, and it lands
    let claimed = claims(&test_box, &remote, &id);
    assert!(
        claimed
            .last()
            .is_some_and(|claim| claim.contains("by exec-b")),
        "runner B waited out the gap of runner A's worker: {claimed:?} {pass}"
    );
    assert!(
        test_box.trunk_change_ids(&remote).contains(&id),
        "the change did not land: {pass}"
    );
}

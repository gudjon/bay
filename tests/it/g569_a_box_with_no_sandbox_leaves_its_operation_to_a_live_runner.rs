//! G569 (a fleet finding, 2026-10-29: ueapozv7). A box with no check sandbox, which has not
//! chosen to run checks without one, runs no check (ruling R14), so it serves no environment:
//! `bay merge --wait` there leaves the operation, unclaimed and in its place, to a live runner
//! that serves it. Before, the box claimed the operation, every check was `infra_failure`, and
//! the operation stayed blocked with `CHECK_UNKNOWN` and no place, which no runner takes.
//!
//! The guard to mutate: `src/publish.rs` `served_elsewhere`'s `unsandboxed` (make it `None`).
//!
//! The sandbox is hidden by a `bwrap` on `PATH` that fails its probe, so this runs on Linux
//! only, with or without a real `bwrap`.

#![cfg(target_os = "linux")]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// A runner clone of `remote` named `name`.
fn runner(test_box: &TestBox, remote: &Path, name: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    clone
}

#[test]
fn a_box_with_no_sandbox_leaves_its_operation_to_a_live_runner() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let check = check_files("", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &check);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let exec = runner(&test_box, &remote, "exec");
    // The runner's presence is fresh: it made a pass, and stopped before it claimed the change.
    let _ = test_box
        .bay_process(&exec)
        .args(["--json", "runner", "run", "--once"])
        .env("BAY_TEST_PAUSE_AT", "before-claim")
        .env("BAY_TEST_PAUSE_FILE", test_box.path().join("never"))
        .env("BAY_TEST_PAUSE_TIMEOUT_S", "2")
        .output();

    test_box.no_sandbox();
    test_box.choose_unsandboxed(false);
    let (code, waited) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{waited}");
    assert_eq!(waited["decision"], "queued", "{waited}");
    assert_eq!(waited["reason"], "CHECK_UNAVAILABLE", "{waited}");
    assert_eq!(waited["waiting_for"], "worker", "{waited}");
    assert!(
        waited["detail"]["sandbox"]
            .as_str()
            .is_some_and(|why| why.contains("bay.runnerSandbox")),
        "the result does not say why this box runs no check: {waited}"
    );
    let chain = test_box.git_bare(
        &remote,
        &["log", "--format=%s", &bay::change::change_ref(&id)],
    );
    assert!(
        !chain.contains("by local") && !chain.contains("blocked"),
        "the box with no sandbox claimed the operation: {chain}"
    );

    // The runner's box has a sandbox, or chose to run without one.
    test_box.choose_unsandboxed(true);
    for _ in 0..3 {
        test_box.bay_ok(&exec, &["runner", "run", "--once"]);
        if test_box.trunk_change_ids(&remote).contains(&id) {
            break;
        }
    }
    assert!(
        test_box.trunk_change_ids(&remote).contains(&id),
        "the live runner did not merge the change: {}",
        test_box.git_bare(
            &remote,
            &["log", "--format=%s", &bay::change::change_ref(&id)]
        )
    );
}

#[test]
fn with_no_live_runner_a_box_with_no_sandbox_still_says_check_unknown() {
    // Ruling R14 as before: no runner serves the environment, so the box's own run is the
    // answer, and it is `infra_failure`, never a pass.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let check = check_files("", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &check);
    let (_id, view) = test_box.start_and_edit(&repo);
    test_box.no_sandbox();
    test_box.choose_unsandboxed(false);
    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "CHECK_UNKNOWN", "{result}");
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());
}

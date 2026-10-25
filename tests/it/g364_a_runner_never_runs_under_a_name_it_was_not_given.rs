//! G364 (a fleet rollout's finding, 2026-10-25: a shared runner ran all day as `local`). A runner
//! never runs under a name it was not given.
//!
//! 0.4.0 renamed `bay.executorName` and its siblings (`crate::signing::RENAMED_KEYS`), and bay
//! never reads an old name. `bay install --machine` moves the global keys, but a runner whose own
//! clone still set the old name published its presence as `local`. Now the runner moves the
//! renamed keys in its own clone when it starts, as `bay install` does, and says so.
//!
//! Guard to mutate: `src/executor.rs` `run`: drop the `carry_over` call (the presence is
//! published as `local`).

use crate::support;

use support::{TestBox, unchecked_files};

#[test]
fn a_runner_whose_clone_names_it_under_the_old_key_runs_under_that_name() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let runner = test_box.clone_of(&remote, "runner");
    test_box.git_ok(&runner, &["config", "--local", "bay.executorName", "box-a"]);

    let out = test_box
        .bay(&runner)
        .args(["runner", "run", "--once"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("moved bay.executorName to bay.runnerName"),
        "the move is not said: {stderr}"
    );
    assert_eq!(
        test_box.git_ok(&runner, &["config", "--local", "bay.runnerName"]),
        "box-a"
    );
    let old = test_box
        .git(&runner)
        .args(["config", "--local", "bay.executorName"])
        .output()
        .unwrap();
    assert!(!old.status.success(), "the old key is still set");
    let presence = test_box.git_bare(
        &remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/executor/"],
    );
    assert_eq!(presence, "refs/bay/executor/box-a", "{presence}");
}

//! G763 (owner, 2026-11-16: "it's a real issue how much disk space Bay actually needs"; one box
//! held 41 GB of check scratch, one tree of 14 GB, all of operations that had ended, some in a
//! store agents' views share, which no runner passes over). A check scratch directory of an
//! operation that ended goes at the next status sweep, whatever its age; one of an operation under
//! way is kept. One a check in a view made (`-own`) goes once its change has ended.
//!
//! Guards to mutate: in `src/executor.rs` `sweep_check_scratch`, drop `scratch_of_ended`, and the
//! ended operation's tree stays; in `src/status.rs`, drop the `sweep_check_scratch` call after the
//! sweep, and the shared store keeps both.

#![cfg(unix)]

use crate::support;

use std::path::Path;

use support::{TestBox, check_files, passing_result, result_json};

/// A check's scratch tree under `dir`.
fn scratch(dir: &Path) {
    std::fs::create_dir_all(dir.join("check-0/candidate")).unwrap();
    std::fs::write(dir.join("check-0/candidate/file"), "content\n").unwrap();
}

#[test]
fn check_scratch_goes_when_its_operation_ends() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let runner = test_box.clone_of(&remote, "runner");
    let mut ids = Vec::new();
    for name in ["ended", "live"] {
        let (id, view) = test_box.start_and_edit(&repo);
        std::fs::write(
            view.join(format!("src/{name}.rs")),
            format!("fn {name}() {{}}\n"),
        )
        .unwrap();
        let (code, sent) = result_json(
            test_box
                .bay(&view)
                .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
                .arg("merge"),
        );
        assert_eq!(code, 0, "{sent}");
        if name == "ended" {
            let ran = test_box.bay_ok(&runner, &["runner", "run", "--once"]);
            assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
        }
        ids.push(id);
    }
    let (ended, live) = (&ids[0], &ids[1]);

    // GIVEN the shared store's scratch of both, as runs and checks in views leave it
    let root =
        bay::check::scratch_root_in(&test_box.home.join(".local/state/bay"), &repo.join(".git"));
    let dirs = [
        root.join(format!("{ended}.1.1")),
        root.join(format!("{ended}.1.1-own")),
        root.join(format!("{live}.1.1")),
        root.join(format!("{live}.1.1-own")),
    ];
    for dir in &dirs {
        scratch(dir);
    }

    // WHEN a status sweep reads the store
    let _ = test_box.bay_ok(&repo, &["status"]);

    // THEN the ended change's trees are gone, and the live one's kept
    assert!(!dirs[0].exists(), "the ended operation's tree stayed");
    assert!(!dirs[1].exists(), "the ended change's own tree stayed");
    assert!(dirs[2].exists(), "the live operation's tree was removed");
    assert!(dirs[3].exists(), "the live change's own tree was removed");
}

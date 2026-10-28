//! G415 (a fleet finding, 2026-10-27: a box's runners held 17 GB of check scratch for
//! operations that had long landed or been abandoned, and the plant's disk fell to 17 GB free).
//! A check's scratch goes on every path out of its run, and a runner's pass removes each scratch
//! directory nothing has touched for six hours, which only a killed or re-executed process leaves.
//! A recent one is kept.
//!
//! The scratch root is the repository's own in bay's state directory (`check::scratch_root`);
//! a bay before 0.5.42 made it under `<common-dir>/bay/check/`, and a stale one there goes too.
//! A stale one goes whole, its read-only adapter copy as well (BACKLOG 114: `remove_dir_all`
//! cannot unlink a file in a read-only directory, so such scratch outlived every removal).
//!
//! Guards to mutate: `src/executor.rs` `pass`: drop the `sweep_check_scratch` call; either root
//! in `sweep_check_scratch`; `src/check.rs` `remove_tree`: drop its `make_writable`.

#![cfg(unix)]

use crate::support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use support::{TestBox, result_json, unchecked_files};

/// A scratch directory as a check leaves it: its tree, and its adapter copy read-only.
fn scratch(dir: &Path) {
    std::fs::create_dir_all(dir.join("check-0/candidate")).unwrap();
    std::fs::write(dir.join("check-0/candidate/file"), "content\n").unwrap();
    std::fs::create_dir_all(dir.join("check-0/adapter/.bay")).unwrap();
    std::fs::write(dir.join("check-0/adapter/.bay/adapter.py"), "print()\n").unwrap();
    for read_only in ["check-0/adapter/.bay", "check-0/adapter"] {
        std::fs::set_permissions(dir.join(read_only), std::fs::Permissions::from_mode(0o555))
            .unwrap();
    }
}

fn age(dir: &Path) {
    let status = std::process::Command::new("touch")
        .args(["-d", "7 hours ago"])
        .arg(dir)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn a_pass_removes_stale_check_scratch_and_keeps_a_recent_one() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let runner = test_box.clone_of(&remote, "runner");
    let root = bay::check::scratch_root_in(
        &test_box.home.join(".local/state/bay"),
        &runner.join(".git"),
    );
    let stale = root.join("batch-left-by-a-killed-run");
    let before = runner.join(".git/bay/check/post-left-by-an-older-bay");
    let recent = root.join("batch-running-now");
    for dir in [&stale, &before, &recent] {
        scratch(dir);
    }
    age(&stale);
    age(&before);
    let (code, pass) = result_json(test_box.bay(&runner).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{pass}");
    assert!(!stale.exists(), "stale scratch is removed whole: {pass}");
    assert!(
        !before.exists(),
        "stale scratch an older bay left in the repository is removed: {pass}"
    );
    assert!(
        recent.join("check-0/candidate/file").exists(),
        "recent scratch is kept: {pass}"
    );
}

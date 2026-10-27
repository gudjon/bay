//! G415 (a fleet finding, 2026-10-27: a box's runners held 17 GB of check scratch for
//! operations that had long landed or been abandoned, and the plant's disk fell to 17 GB free).
//! A check's scratch goes on every path out of its run, and a runner's pass removes each scratch
//! directory nothing has touched for six hours, which only a killed or re-executed process leaves.
//! A recent one is kept.
//!
//! Guard to mutate: `src/executor.rs` `pass`: drop the `sweep_check_scratch` call.

#![cfg(unix)]

use crate::support;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_pass_removes_stale_check_scratch_and_keeps_a_recent_one() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let runner = test_box.clone_of(&remote, "runner");
    let check = runner.join(".git/bay/check");
    let stale = check.join("batch-left-by-a-killed-run");
    let recent = check.join("batch-running-now");
    for dir in [&stale, &recent] {
        std::fs::create_dir_all(dir.join("tree")).unwrap();
        std::fs::write(dir.join("tree/file"), "content\n").unwrap();
    }
    let status = std::process::Command::new("touch")
        .args(["-d", "7 hours ago"])
        .arg(&stale)
        .status()
        .unwrap();
    assert!(status.success());
    let (code, pass) = result_json(test_box.bay(&runner).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{pass}");
    assert!(!stale.exists(), "stale scratch is removed: {pass}");
    assert!(
        recent.join("tree/file").exists(),
        "recent scratch is kept: {pass}"
    );
}

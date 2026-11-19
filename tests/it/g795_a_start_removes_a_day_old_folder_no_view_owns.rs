//! G795 (a fleet finding, 2026-11-19: a folder held 6.4 GB of build output for six days in a
//! repository's views directory: a build still running in a view wrote it again after the view's
//! change ended and the view was removed, and nothing named it any more). A start removes a
//! folder in the views directory that no journal names and no worktree is, once nothing has
//! touched it for a day. A newer one, and every view, stay.
//!
//! Guard to mutate: in `src/start.rs` `reclaim`, drop the `sweep_orphans` call (the old folder
//! stays).

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::{TestBox, check_files, passing_result};

#[test]
fn a_start_removes_an_old_folder_no_view_owns_and_keeps_the_rest() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (first, view) = test_box.start_and_edit(&repo);
    let views: PathBuf = view.parent().unwrap().to_path_buf();

    // A build's output left where a view was, two days old; and one written a moment ago.
    let old = views.join("zzzzold1");
    std::fs::create_dir_all(old.join("target/debug")).unwrap();
    std::fs::write(old.join("target/debug/out.bin"), b"built").unwrap();
    let touched = std::process::Command::new("touch")
        .args(["-t", "202001010000"])
        .arg(&old)
        .status()
        .unwrap();
    assert!(touched.success());
    let fresh = views.join("zzzznew1");
    std::fs::create_dir_all(fresh.join("target")).unwrap();

    // Doctor names it among what the next start frees.
    let (_, doctor) = support::result_json(test_box.bay(&repo).arg("doctor"));
    let frees = support::row(&doctor, "storage")["next_start_frees"]["views"].to_string();
    assert!(frees.contains("zzzzold1"), "{frees}");
    assert!(!frees.contains("zzzznew1"), "{frees}");

    // WHEN another change starts
    let started = test_box.bay_ok(&repo, &["start", "another", "--paths", "src", "--new"]);

    // THEN the old folder is gone, and the fresh one and the first change's view stay
    assert!(!old.exists(), "the old folder stayed: {started}");
    assert!(fresh.exists(), "a fresh folder went: {started}");
    assert!(view.exists(), "the view of {first} went: {started}");
}

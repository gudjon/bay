//! G848 (a fleet finding, 2026-11-21: a store lost objects, and its tracking ref of main named a
//! commit it no longer held). bay read the config at that ref, found
//! no commit, and said `CONFIG_MISSING`: bay is not set up in this project yet. The store is
//! damaged, not unset: a command now fails and names the ref and the commit the store lacks.
//!
//! Guard to mutate: in `src/repo.rs` `open_reading`, drop the G848 check of a ref whose commit
//! is lost; the store then reads as `CONFIG_MISSING`.

#![cfg(unix)]

use crate::support;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_tracking_ref_naming_a_commit_the_store_lost_is_a_damaged_store() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());

    // GIVEN main's tracking ref names a commit whose object is gone from the store
    let tree = test_box.git_ok(&repo, &["rev-parse", "refs/remotes/origin/main^{tree}"]);
    let lost = test_box.git_ok(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t.invalid",
            "commit-tree",
            &tree,
            "-p",
            "refs/remotes/origin/main",
            "-m",
            "lost",
        ],
    );
    test_box.git_ok(&repo, &["update-ref", "refs/remotes/origin/main", &lost]);
    let object = repo.join(".git/objects").join(&lost[..2]).join(&lost[2..]);
    std::fs::remove_file(&object).unwrap();
    // AND a remote this store cannot reach, so no fetch brings main again
    let gone = test_box.path().join("gone.git");
    test_box.git_ok(
        &repo,
        &["remote", "set-url", "origin", gone.to_str().unwrap()],
    );

    // WHEN a command reads the config
    let (code, result) = result_json(test_box.bay(&repo).arg("status"));

    // THEN it fails and names the commit the store lacks, never CONFIG_MISSING
    assert_eq!(code, 20, "{result}");
    assert_eq!(result["decision"], "failed", "{result}");
    assert_ne!(result["reason"], "CONFIG_MISSING", "{result}");
    let detail = result["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("the store is damaged") && detail.contains(&lost),
        "{result}"
    );
}

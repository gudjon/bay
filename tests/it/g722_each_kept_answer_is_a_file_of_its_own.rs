//! G722 (a measurement, 2026-11-15: in one hour the single file of kept answers (G721) grew to
//! 15 MB, and every process read it whole at its first ask). Each kept answer is a file of its
//! own, named by its key, so an ask opens one file however many are kept; the single file an
//! earlier bay kept is removed by the next write.
//!
//! Guard to mutate: `src/facts.rs` `path`: name every answer by one file (the store then holds
//! one answer file).

#![cfg(unix)]

use crate::support;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_listing_keeps_each_answer_in_a_file_of_its_own() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let _ = test_box.start_and_edit(&repo);
    // GIVEN the single file of answers an earlier bay kept
    let old = repo.join(".git/bay/facts");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, b"answers an earlier bay kept").unwrap();

    // WHEN a listing reads the store whole
    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .arg("status")
            .env("BAY_TEST_SAVED", "off"),
    );
    assert_eq!(code, 0, "{result}");
    support::sweeps_ended(&repo);

    // THEN each answer it kept is a file of its own, and the old file is gone
    let kept: Vec<_> = walk(&repo.join(".git/bay/answers"));
    assert!(kept.len() > 1, "kept answers: {kept:?}");
    assert!(!old.exists(), "the file an earlier bay kept is still there");
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

//! G27. Sparse absence is never a deletion.
//!
//! A view with paths not materialized builds a candidate with no deletions.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_sparse_view_publishes_no_deletions() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("docs/guide.md", "guide\n"),
            ("data/big.csv", "1,2\n"),
        ],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (_, view) = test_box.start_and_edit(&repo);
    assert!(!view.join("docs/guide.md").exists() && !view.join("data/big.csv").exists());
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let changes = test_box.git_bare(&remote, &["diff", "--name-status", "main^", "main"]);
    assert_eq!(
        changes, "A\tsrc/new.rs",
        "only the edit, no deletions: {changes}"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:docs/guide.md"]),
        "guide"
    );
}

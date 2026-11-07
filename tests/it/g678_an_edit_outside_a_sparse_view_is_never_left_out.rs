//! G678 (a move of 4,154 files, 2026-11-07: 12 files edited outside the view's sparse set were
//! missing from a build, found only when a check failed; the agent's own `git add` had skipped
//! them). A view's save stages every edit, outside the sparse set too, so the change holds what the
//! agent wrote, whatever it staged itself.
//!
//! Guard to mutate, in `src/change.rs` `freeze_draft`: `--sparse` on its `git add` (the edit is
//! lost and main keeps the old text).

#![cfg(unix)]

use crate::support;

use support::{TestBox, UNCHECKED_POLICY, result_json};

#[test]
fn a_file_written_outside_the_sparse_set_reaches_main() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "old\n")],
        &[(".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned())],
    );
    let started = test_box.bay_ok(&repo, &["start", "repoint", "--paths", "src/"]);
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    assert!(
        !view.join("docs/a.md").exists(),
        "docs/ is outside the view's set"
    );
    std::fs::create_dir_all(view.join("docs")).unwrap();
    std::fs::write(view.join("docs/a.md"), "new\n").unwrap();
    std::fs::write(view.join("src/a.rs"), "fn a() { }\n").unwrap();

    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    assert_eq!(code, 0, "{merged}");
    let remote = test_box.path().join("repo-remote.git");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:docs/a.md"]),
        "new"
    );
}

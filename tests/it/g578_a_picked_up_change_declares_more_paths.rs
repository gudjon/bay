//! G578 (BACKLOG 143, a fleet finding, 2026-10-30: `bay start <change> --paths` kept the change's
//! old paths, so a change that needed one more path was given up for a new change at the back of
//! the line). A pick-up with `--paths` declares each path the change does not cover yet: its view
//! holds them, beside the work it had, and the change merges with a file under one.
//!
//! Guards to mutate: `src/start.rs` `declare_paths` returning no path (the pick-up names no
//! added path, and the view never holds `docs/`); `src/view.rs` `keep_declared` returning before
//! its `sparse-checkout add` (the change declares `docs/`, and its view does not hold it).

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::json;
use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_picked_up_change_declares_more_paths_and_merges_with_them() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    // GIVEN a change on src/, with work in its view.
    let started = test_box.bay_ok(&repo, &["start", "CARD-578", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    assert!(!view.join("docs/a.md").exists(), "a view holds only src/");
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();

    // WHEN it is picked up with docs/ too
    let (code, widened) = result_json(
        test_box
            .bay(&repo)
            .args(["start", &id, "--paths", "src/", "docs/"]),
    );
    assert_eq!(code, 0, "{widened}");
    assert_eq!(widened["change"], id.as_str(), "{widened}");
    assert_eq!(
        widened["detail"]["paths_added"],
        json!(["docs/"]),
        "{widened}"
    );
    // THEN its view holds docs/, beside the work it had
    assert!(
        view.join("docs/a.md").exists(),
        "the view does not hold docs/: {widened}"
    );
    assert!(view.join("src/new.rs").exists(), "{widened}");
    // AND a pick-up with a path it covers adds none.
    let (code, again) =
        result_json(
            test_box
                .bay(&repo)
                .args(["start", &id, "--paths", "docs/a.md"]),
        );
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["detail"]["paths_added"], json!([]), "{again}");

    // AND the change merges with a file under docs/.
    std::fs::write(view.join("docs/b.md"), "b\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
    let merged = test_box.git_bare(&remote, &["show", "main:docs/b.md"]);
    assert_eq!(merged.trim(), "b", "{landed}");
}

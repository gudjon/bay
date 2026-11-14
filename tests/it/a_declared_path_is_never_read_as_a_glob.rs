//! BACKLOG 236 (a fleet finding, 2026-11-14, bay 0.6.70: change 6p73hr2t declared
//! `apps/cloud/studio/src/app/(dashboard)/[org]/[venue]/calibration`, and its view did not hold
//! the folder). `bay start --paths` wrote each declared path into the non-cone sparse set as it
//! was, and git reads a non-cone pattern as a glob: `[org]` was a class of one character, so the
//! view held `app/o/v/cal/` and not `app/[org]/[venue]/cal/`. Every Next.js dynamic-route folder
//! meets this. A declared path is a path: its view holds that folder and no other, and the change
//! merges with a file under it.
//!
//! Guard to mutate: `src/view.rs` `sparse_pattern` pushing no `\` before a character git reads as
//! a glob (the view holds `app/o/v/cal/` and not the declared folder).

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::json;
use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_declared_folder_with_brackets_is_held_and_no_folder_its_glob_matches() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[
            ("app/[org]/[venue]/cal/page.tsx", "declared\n"),
            ("app/o/v/cal/page.tsx", "a folder the glob matches\n"),
            ("src/a.rs", "fn a() {}\n"),
        ],
        &check_files("true", &passing_result(1), 1, 60),
    );

    // WHEN a change declares the dynamic-route folder
    let started = test_box.bay_ok(
        &repo,
        &["start", "CARD-236", "--paths", "app/[org]/[venue]/cal/"],
    );
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());

    // THEN its view holds that folder, and no folder the path would match as a glob
    assert!(
        view.join("app/[org]/[venue]/cal/page.tsx").exists(),
        "the view does not hold the declared folder: {started}"
    );
    assert!(
        !view.join("app/o/v/cal/page.tsx").exists(),
        "the view holds a folder the declared path matches only as a glob: {started}"
    );
    assert!(!view.join("src/a.rs").exists(), "{started}");

    // AND a pick-up with the same path finds it held and adds none
    let (code, again) =
        result_json(
            test_box
                .bay(&repo)
                .args(["start", &id, "--paths", "app/[org]/[venue]/cal/"]),
        );
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["detail"]["paths_added"], json!([]), "{again}");

    // AND the change merges with a file under the folder.
    std::fs::write(view.join("app/[org]/[venue]/cal/page.tsx"), "edited\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
    let merged = test_box.git_bare(&remote, &["show", "main:app/[org]/[venue]/cal/page.tsx"]);
    assert_eq!(merged.trim(), "edited", "{landed}");
    let untouched = test_box.git_bare(&remote, &["show", "main:app/o/v/cal/page.tsx"]);
    assert_eq!(untouched.trim(), "a folder the glob matches", "{landed}");
}

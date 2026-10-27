//! G416 (fleet findings, 2026-10-27: a refresh's new base was recorded only in a chain bay then
//! set aside, and a seat merged main into its view by hand; in both, the change's draft against
//! its recorded base carried other changes' files, it was refused for files it never touched,
//! and every refresh conflicted). A view that stands on a newer main than its change records is
//! judged from the main it stands on: its draft holds only its own work.
//!
//! Guard to mutate: `src/publish.rs` `submit_land`: drop the `stand_on_view_base` call.

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_view_with_main_merged_in_lands_only_its_own_work() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/b.md", "b\n")],
        &unchecked_files(),
    );
    let started = test_box.bay_ok(&repo, &["start", "CARD-SRC", "--paths", "src/"]);
    let mine = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/mine.rs"), "fn mine() {}\n").unwrap();

    // Another change lands on docs/ meanwhile.
    let other = test_box.bay_ok(&repo, &["start", "CARD-DOCS", "--paths", "docs/"]);
    let other_view = PathBuf::from(other["detail"]["view"].as_str().unwrap());
    std::fs::write(other_view.join("docs/b.md"), "b, newer\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&other_view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let newer = test_box.git_bare(&remote, &["rev-parse", "refs/heads/main"]);

    // The author brings that main into the view by hand.
    test_box.git_ok(&view, &["add", "--all", "--sparse"]);
    test_box.git_ok(&view, &["commit", "-q", "-m", "mine"]);
    test_box.git_ok(&view, &["merge", "-q", "--no-edit", newer.trim()]);
    // Main moves on again in the file the change never touched; a draft read against the old
    // base would carry the older version of it and conflict there.
    let third = test_box.bay_ok(&repo, &["start", "CARD-DOCS-2", "--paths", "docs/"]);
    let third_view = PathBuf::from(third["detail"]["view"].as_str().unwrap());
    std::fs::write(third_view.join("docs/b.md"), "b, newest\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&third_view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");

    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    let ids = test_box.trunk_change_ids(&remote);
    assert!(ids.contains(&mine), "{ids:?} {merged}");
    let tip = test_box.git_bare(&remote, &["rev-parse", "refs/heads/main"]);
    let touched = test_box.git_bare(
        &remote,
        &[
            "diff",
            "--name-only",
            &format!("{}^1", tip.trim()),
            tip.trim(),
        ],
    );
    assert_eq!(touched.trim(), "src/mine.rs", "{merged}");
}

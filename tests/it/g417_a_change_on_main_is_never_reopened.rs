//! G417 (a fleet finding, 2026-10-27: `bay start` on a change already on main reopened it,
//! `bay merge` queued a new edit behind the merged revision, and a runner later refused that
//! edit as already merged with no word to its author, so the work quietly went nowhere). A change
//! on main takes no new revision and is never reopened: both are refused at once, saying so, and
//! the new edit stays in the view.
//!
//! Guard to mutate: `src/start.rs` `resume`: drop the landed check (a merge of a new edit on a
//! landed change was refused already; the edit got queued only once a start had reopened it).

#![cfg(unix)]

use crate::support;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_new_edit_on_a_merged_change_is_refused_at_once_and_kept() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");

    // A new edit in the merged change's view.
    std::fs::write(view.join("src/later.rs"), "fn later() {}\n").unwrap();
    let (code, refused) = result_json(test_box.bay(&view).arg("merge"));
    assert_ne!(code, 0, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_ALREADY_MERGED", "{refused}");
    assert!(
        view.join("src/later.rs").exists(),
        "the new edit stays in the view"
    );

    let (code, reopened) = result_json(test_box.bay(&repo).args(["start", &id]));
    assert_ne!(code, 0, "{reopened}");
    assert_eq!(reopened["reason"], "CHANGE_ALREADY_MERGED", "{reopened}");
    assert!(view.join("src/later.rs").exists(), "the edit is still kept");
}

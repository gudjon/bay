//! G405 (a fleet finding, 2026-10-26): a full disk left two views' journals unfinished. Their
//! submits were refused `VIEW_INCOMPLETE`, and the repair the refusal names, `bay start
//! <change>`, removed each view, whole, before any checkpoint, then made it again from the
//! draft before the agent's edits. bay never deletes work that no draft holds: a view it
//! must make again is moved aside when it holds such work, and the result says where.
//!
//! Guard to mutate: `src/view.rs` `discard`: delete the view whatever it holds.

use crate::support;

use std::path::PathBuf;

use support::TestBox;

#[test]
fn a_view_whose_journal_was_lost_keeps_its_unsaved_edits() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/a.rs"), "fn a() { edited }\n").unwrap();
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let journal = repo.join(".git/bay/views").join(format!("{id}.journal"));
    assert!(journal.exists(), "{journal:?}");
    std::fs::remove_file(&journal).unwrap();

    let resumed = test_box.bay_ok(&repo, &["start", &id]);
    let kept = PathBuf::from(
        resumed["detail"]["unsaved_kept"]
            .as_str()
            .unwrap_or_else(|| panic!("no unsaved work was kept: {resumed}")),
    );
    assert_eq!(
        std::fs::read_to_string(kept.join("src/a.rs")).unwrap(),
        "fn a() { edited }\n"
    );
    assert_eq!(
        std::fs::read_to_string(kept.join("src/new.rs")).unwrap(),
        "fn new() {}\n"
    );
}

/// A view that holds nothing newer than its draft is made again with nothing kept aside.
#[test]
fn a_view_with_nothing_unsaved_is_made_again_with_nothing_kept() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let journal = repo.join(".git/bay/views").join(format!("{id}.journal"));
    std::fs::remove_file(&journal).unwrap();
    let resumed = test_box.bay_ok(&repo, &["start", &id]);
    assert!(resumed["detail"]["unsaved_kept"].is_null(), "{resumed}");
}

//! G745 (a fleet finding, 2026-11-15: after `bay abandon`, its own actor's `bay start` of the same
//! intent answered ready with the change still abandoned, and the same start released its view as
//! an ended change's, so the folder it named was gone). A start by a change's own actor reopens it
//! when it was abandoned, as an adoption does: the change is open again and its view stays.
//!
//! Guard to mutate: in `src/start.rs`, drop `reopens` from the choice of the adoption branch, and
//! the change stays abandoned.

use crate::support;

use std::path::PathBuf;

use support::{TestBox, unchecked_files};

fn state(test_box: &TestBox, repo: &std::path::Path, id: &str) -> String {
    test_box.bay_ok(repo, &["status", id])["detail"]["changes"][0]["state"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn a_start_after_its_own_abandon_reopens_the_change_and_keeps_its_view() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    // GIVEN a change its own actor abandoned
    let first = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let id = first["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(first["detail"]["view"].as_str().unwrap());
    test_box.bay_ok(&view, &["abandon"]);
    assert_eq!(state(&test_box, &repo, &id), "abandoned");

    // WHEN the same actor starts the same intent again
    let again = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);

    // THEN it is the same change, open again, and the folder it names is there
    assert_eq!(again["change"], id.as_str(), "{again}");
    assert_eq!(state(&test_box, &repo, &id), "open");
    let named = PathBuf::from(again["detail"]["view"].as_str().unwrap());
    assert!(named.exists(), "the view it named is gone: {again}");

    // AND a start by its id after another abandon does the same
    test_box.bay_ok(&named, &["abandon"]);
    test_box.bay_ok(&repo, &["start", &id]);
    assert_eq!(state(&test_box, &repo, &id), "open");
}

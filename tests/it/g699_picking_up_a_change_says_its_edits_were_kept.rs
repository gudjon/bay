//! G699 (a fleet finding, 2026-11-12: an agent picked up its change with `bay start <change>`,
//! which saved its edits and moved them onto newer main, and said only "Ready"; its next
//! `git status` looked as if files were missing). Picking up a change whose edits were saved
//! first says so, and says when they now sit on newer main.
//!
//! Guard to mutate: in `src/result.rs` `ok_say`, the sentence for `saved_first && moved` (the
//! pick-up says only "Ready").

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::{TestBox, unchecked_files};

#[test]
fn picking_up_a_change_says_its_edits_were_kept() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    // GIVEN a change with edits in its view, and main moved on by another change
    let (id, _view) = test_box.start_and_edit(&repo);
    let other = test_box.bay_ok(&repo, &["start", "OTHER", "--paths", "src/"]);
    let other_view = PathBuf::from(other["detail"]["view"].as_str().unwrap());
    std::fs::write(other_view.join("src/b.rs"), "fn b() {}\n").unwrap();
    let merged = test_box.bay_ok(&other_view, &["merge", "--wait"]);
    assert_eq!(merged["decision"], "merged", "{merged}");

    // WHEN its actor picks it up again
    let picked = test_box.bay_ok(&repo, &["start", &id]);

    // THEN the sentence says its edits were kept and now sit on the newest main
    assert_eq!(picked["detail"]["checkpointed"], true, "{picked}");
    let say = picked["say"].as_str().unwrap();
    assert!(say.contains("kept") && say.contains("newest main"), "{say}");
}

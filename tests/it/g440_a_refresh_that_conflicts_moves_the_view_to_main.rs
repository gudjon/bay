//! G440. A refresh that conflicts moves the view to main, with the conflicts marked.
//!
//! `bay start <change>` brings in newer main, and the skill's fix in place is "start, fix it in
//! the view, merge". A refresh whose merge onto main conflicted refused and left the view on its
//! old base with no markers, so a repair made there was merged three-way from the old base again
//! and conflicted again (2026-10-28: 3udwqnca; kmi6sqbk .1 and .2, two changes that appended
//! tests in one place). Now the view moves onto main with the conflict markers in the listed
//! paths, and `bay merge` refuses a change that still adds the markers bay wrote.
//!
//! The guards to mutate: the conflict arm of `start::bring_in_trunk` (`src/start.rs`; the view
//! stays on the old base and holds no markers) and `markers_left_at_submit` in `submit_land`
//! (`src/publish.rs`; the unresolved merge is not refused and the markers reach main).

// Each case runs a shell adapter, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_conflicting_refresh_is_repaired_against_main_in_place() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("tests/list.txt", "a\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    // Two changes append a line in the same place; the second one lands first.
    let mine = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "tests/"]);
    let id = mine["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(mine["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("tests/list.txt"), "a\nmine\n").unwrap();
    let theirs = test_box.bay_ok(&repo, &["start", "CARD-2", "--paths", "tests/"]);
    let their_view = PathBuf::from(theirs["detail"]["view"].as_str().unwrap());
    std::fs::write(their_view.join("tests/list.txt"), "a\ntheirs\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&their_view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");

    let (code, refused) = result_json(test_box.bay(&repo).args(["start", &id]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "MERGE_CONFLICT", "{refused}");
    assert_eq!(
        refused["detail"]["conflicts"][0], "tests/list.txt",
        "{refused}"
    );
    let text = std::fs::read_to_string(view.join("tests/list.txt")).unwrap();
    assert!(
        text.contains("<<<<<<<") && text.contains("theirs") && text.contains("mine"),
        "the view was left on its old base: {text}"
    );

    // A merge with the markers still in the file is refused; nothing reaches main.
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let (code, unresolved) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{unresolved}");
    assert_eq!(unresolved["reason"], "MERGE_CONFLICT", "{unresolved}");
    assert_eq!(
        unresolved["detail"]["conflicts"][0], "tests/list.txt",
        "{unresolved}"
    );
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);

    // The repair, made against main in the view, lands with both lines.
    std::fs::write(view.join("tests/list.txt"), "a\ntheirs\nmine\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:tests/list.txt"]),
        "a\ntheirs\nmine",
        "git_bare trims its output"
    );
}

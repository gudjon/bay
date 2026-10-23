//! G338 (BACKLOG 51 R4, owner ruling 2026-10-22): `bay save` takes what the work is, in a few
//! words.
//!
//! A person who saves does not have a card, issue or ticket id; they say what they did. So
//! `bay save "<what it is>"` takes a phrase with spaces, its help and its refusal ask for "what
//! this work is, in a few words (or a card id)", and saving again with the same words goes on
//! with the same change: a save whose check failed is picked up by the next save of that work,
//! never a second change beside it.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/main.rs` `Command::Save`'s `intent` doc: back to "The intent: a card, issue or ticket
//!   id" (the help names a ticket).
//! - `src/save.rs` `save_in`'s empty-intent refusal: back to "an intent: a card, issue or ticket
//!   id" (the refusal names a ticket).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json, unchecked_files};

const WORK: &str = "make the logo bigger";

#[test]
fn a_phrase_saves_and_saving_it_again_goes_on_with_the_same_change() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    test_box.seed_with_check(
        &repo,
        &[("art/logo.txt", "small\n")],
        &check_files(
            &format!("[ -e art/broken ] && {{ {failing}; exit 0; }}"),
            &passing_result(1),
            1,
            60,
        ),
    );
    std::fs::write(repo.join("art/logo.txt"), "big\n").unwrap();
    std::fs::write(repo.join("art/broken"), "fails the check\n").unwrap();
    let (code, refused) = result_json(test_box.bay(&repo).args(["save", WORK]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    let first = refused["change"]
        .as_str()
        .expect("the change is named")
        .to_owned();

    std::fs::remove_file(repo.join("art/broken")).unwrap();
    let (code, saved) = result_json(test_box.bay(&repo).args(["save", WORK]));
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    assert_eq!(
        saved["change"],
        first.as_str(),
        "a second change beside the first: {saved}"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:art/logo.txt"]),
        "big"
    );
}

#[test]
fn the_help_and_the_refusal_ask_what_the_work_is() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let out = test_box
        .bay(&repo)
        .args(["save", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(help.contains("in a few words"), "{help}");
    assert!(!help.contains("ticket"), "{help}");
    let (code, refused) = result_json(test_box.bay(&repo).args(["save", " "]));
    assert_ne!(code, 0, "{refused}");
    let why = refused["detail"].as_str().unwrap_or_default();
    assert!(why.contains("in a few words"), "{refused}");
    assert!(!why.contains("ticket"), "{refused}");
}

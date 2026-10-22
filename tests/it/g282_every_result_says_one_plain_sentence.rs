//! G282 (owner ruling 2026-10-21). Every result says one plain sentence.
//!
//! Every `bay.result/1` carries `say`: one sentence for a person who does not know Git, made
//! from the decision, the reason and the detail in `src/result.rs`, never at a call site. It
//! says what happened and the next step in about 25 words at most, with none of the words
//! "branch", "commit", "merge", "trunk", "rebase" or "ref", in any inflection. The human
//! rendering on stderr prints it first. Driven here through a start, a landing, a queued
//! land, a failed check, a conflict and a failure.
//!
//! Guards to mutate: in `src/result.rs`, `impl Serialize for BayResult`'s `say: &say,` (make it
//! `say: "",`: every case fails on an empty sentence); the `Self::CheckFailed` arm of
//! `Reason::say` (put "commit" in its sentence: the failed-check case fails on the word); and
//! `to_human`'s `vec![self.say(), ...]` (drop `self.say()`: the human-rendering case fails).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// The words a person who does not know Git need not know.
const UNPLAIN: &[&str] = &["branch", "commit", "merge", "trunk", "rebase", "ref"];

/// Whether `word` is one of [`UNPLAIN`] or an inflection of one.
fn unplain(word: &str) -> bool {
    let word = word.to_ascii_lowercase();
    UNPLAIN.iter().any(|&stem| {
        let bare = stem.strip_suffix('e').unwrap_or(stem);
        [stem, bare].iter().any(|start| {
            word.strip_prefix(start).is_some_and(|rest| {
                ["", "s", "es", "d", "ed", "ing", "ted", "ting"].contains(&rest)
            })
        })
    })
}

/// `result` says one plain sentence, and returns it.
fn assert_says(result: &Value, what: &str) -> String {
    let say = result["say"]
        .as_str()
        .unwrap_or_else(|| panic!("{what}: no say: {result}"));
    assert!(!say.trim().is_empty(), "{what}: an empty say: {result}");
    let count = say.split_whitespace().count();
    assert!(count <= 30, "{what}: {count} words: {say}");
    for word in say
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|word| !word.is_empty())
    {
        assert!(!unplain(word), "{what}: {word:?} in {say:?}");
    }
    say.to_owned()
}

/// A repository whose trunk holds `src/a.rs` and one required check that prints `result`.
fn seeded(test_box: &TestBox, result: &str) -> PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", result, 1, 60),
    );
    repo
}

/// `bay start <intent> --paths src/`: its result and its view.
fn start(test_box: &TestBox, repo: &Path, intent: &str) -> (Value, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    (started, view)
}

#[test]
fn a_start_a_landing_and_a_queued_land_say_a_plain_sentence() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, &passing_result(1));

    let (started, view) = start(&test_box, &repo, "CARD-1");
    assert_says(&started, "start");
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let say = assert_says(&landed, "landed");
    // Ruling 2026-10-22 (ch. 35 R2): a person's word for the trunk is "main".
    assert!(say.contains("on main"), "{say}");

    let (_, view) = start(&test_box, &repo, "CARD-2");
    std::fs::write(view.join("src/later.rs"), "fn later() {}\n").unwrap();
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");
    assert_says(&queued, "queued");
}

#[test]
fn a_failed_check_names_the_check_plainly() {
    let test_box = TestBox::new();
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let repo = seeded(&test_box, &failing);
    let (_, view) = start(&test_box, &repo, "CARD-1");
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    let say = assert_says(&refused, "CHECK_FAILED");
    assert!(say.contains("(tests)"), "names the check: {say}");
}

#[test]
fn a_conflict_names_the_files_plainly() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, &passing_result(1));
    let remote = test_box.path().join("repo-remote.git");
    let (_, view) = start(&test_box, &repo, "CARD-1");
    std::fs::write(view.join("src/a.rs"), "fn a() { mine }\n").unwrap();
    // Someone else changes the same line on main.
    let other = test_box.clone_of(&remote, "other");
    std::fs::write(other.join("src/a.rs"), "fn a() { theirs }\n").unwrap();
    test_box.git_ok(&other, &["commit", "-q", "-am", "theirs"]);
    test_box.git_ok(&other, &["push", "-q", "origin", "main"]);

    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "MERGE_CONFLICT", "{refused}");
    let say = assert_says(&refused, "MERGE_CONFLICT");
    assert!(say.contains("src/a.rs"), "names the file: {say}");
}

#[test]
fn a_failure_says_what_went_wrong_and_the_human_rendering_says_it_first() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, &passing_result(1));
    // `bay merge` outside any change's view, naming none, is a failure.
    let (code, failed) = result_json(test_box.bay(&repo).arg("merge"));
    assert_eq!(code, 20, "{failed}");
    assert_eq!(failed["decision"], "failed", "{failed}");
    let say = assert_says(&failed, "failed");
    assert!(say.starts_with("Something went wrong"), "{say}");

    let out = test_box.bay(&repo).arg("merge").output().unwrap();
    let human = String::from_utf8(out.stderr).unwrap();
    let lines: Vec<&str> = human.lines().collect();
    let at = lines
        .iter()
        .position(|line| *line == say)
        .unwrap_or_else(|| panic!("the sentence is not rendered: {human}"));
    assert_eq!(
        lines.get(at + 1).copied(),
        Some("decision: failed"),
        "the sentence comes first: {human}"
    );
}

/// A fault's sentence is repeated whole or not at all: cut short, "... are not both set" would
/// read "... are", and say the opposite.
#[test]
fn a_long_refusal_is_never_cut_into_its_opposite() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, "pass");
    let (code, refused) = result_json(test_box.bay(&repo).args(["approve", "abcd2345"]));
    assert_eq!(code, 20, "{refused}");
    let say = refused["say"].as_str().unwrap_or_default();
    assert!(
        say.contains("not both set") || !say.contains("approverKey"),
        "a cut sentence flips its meaning: {say}"
    );
}

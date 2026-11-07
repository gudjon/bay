//! G668 (a fleet finding, 2026-11-06: one busy repository edited its policy 58 times in a week, and
//! two unrelated edits collided on the `revision` line, twice in one hour). `revision` only says the
//! policy changed. When a change raised it and main raised it too, that is no disagreement: bay merges
//! the rest of the file and writes the next number. A real conflict elsewhere in the file stays one,
//! and a change that did not raise it merges with the next number too (G675).
//!
//! Guards to mutate, in `src/merge.rs` `settle_revision`: the whole function (both collisions fail);
//! the merge of the rest of the file
//! (the real conflict merges with one side lost); the next number (the same-number case).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json};

const POLICY: &str = "revision = 5\nunchecked = true\n\n# first\n\n# middle\n\n# last\n";

fn seeded() -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let files = vec![(".bay/policy.toml".to_owned(), POLICY.to_owned())];
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (test_box, repo)
}

fn start(test_box: &TestBox, repo: &Path, intent: &str) -> PathBuf {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", ".bay/"]);
    PathBuf::from(started["detail"]["view"].as_str().unwrap())
}

fn edit(view: &Path, replace: (&str, &str), revision: u32) {
    let path = view.join(".bay/policy.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains(replace.0), "{text}");
    let text = text
        .replace(replace.0, replace.1)
        .replace("revision = 5", &format!("revision = {revision}"));
    std::fs::write(path, text).unwrap();
}

fn merge(test_box: &TestBox, view: &Path) -> (i32, Value) {
    result_json(test_box.bay(view).args(["merge", "--wait"]))
}

fn main_policy(test_box: &TestBox, repo: &Path) -> String {
    test_box.git_ok(repo, &["fetch", "-q", "origin", "main"]);
    test_box.git_ok(repo, &["show", "origin/main:.bay/policy.toml"])
}

#[test]
fn two_unrelated_edits_that_raise_it_to_different_numbers_both_merge() {
    let (test_box, repo) = seeded();
    let (one, two) = (
        start(&test_box, &repo, "ONE"),
        start(&test_box, &repo, "TWO"),
    );
    edit(&one, ("# first", "# first, edited"), 6);
    edit(&two, ("# last", "# last, edited"), 7);

    let (code, merged) = merge(&test_box, &one);
    assert_eq!(code, 0, "{merged}");
    let (code, merged) = merge(&test_box, &two);
    assert_eq!(code, 0, "{merged}");

    let policy = main_policy(&test_box, &repo);
    assert!(policy.contains("revision = 7"), "{policy}");
    assert!(policy.contains("# first, edited"), "{policy}");
    assert!(policy.contains("# last, edited"), "{policy}");
}

#[test]
fn two_unrelated_edits_that_raise_it_to_the_same_number_both_merge() {
    let (test_box, repo) = seeded();
    let (one, two) = (
        start(&test_box, &repo, "ONE"),
        start(&test_box, &repo, "TWO"),
    );
    edit(&one, ("# first", "# first, edited"), 6);
    edit(&two, ("# last", "# last, edited"), 6);

    let (code, merged) = merge(&test_box, &one);
    assert_eq!(code, 0, "{merged}");
    let (code, merged) = merge(&test_box, &two);
    assert_eq!(code, 0, "{merged}");

    let policy = main_policy(&test_box, &repo);
    assert!(policy.contains("revision = 7"), "the next number: {policy}");
    assert!(policy.contains("# first, edited"), "{policy}");
    assert!(policy.contains("# last, edited"), "{policy}");
}

#[test]
fn a_real_conflict_elsewhere_in_the_file_stays_one() {
    let (test_box, repo) = seeded();
    let (one, two) = (
        start(&test_box, &repo, "ONE"),
        start(&test_box, &repo, "TWO"),
    );
    edit(&one, ("# middle", "# middle, one way"), 6);
    edit(&two, ("# middle", "# middle, the other way"), 7);

    let (code, merged) = merge(&test_box, &one);
    assert_eq!(code, 0, "{merged}");
    let (code, refused) = merge(&test_box, &two);

    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "MERGE_CONFLICT", "{refused}");
    assert!(!main_policy(&test_box, &repo).contains("the other way"));
}

#[test]
fn a_change_that_did_not_raise_it_merges_with_the_next_number() {
    let (test_box, repo) = seeded();
    let (one, two) = (
        start(&test_box, &repo, "ONE"),
        start(&test_box, &repo, "TWO"),
    );
    edit(&one, ("# first", "# first, edited"), 6);
    edit(&two, ("# last", "# last, edited"), 5);

    let (code, merged) = merge(&test_box, &one);
    assert_eq!(code, 0, "{merged}");
    let (code, merged) = merge(&test_box, &two);

    assert_eq!(code, 0, "{merged}");
    let policy = main_policy(&test_box, &repo);
    assert!(
        policy.contains("# first, edited") && policy.contains("# last, edited"),
        "{policy}"
    );
    assert!(policy.starts_with("revision = 7\n"), "{policy}");
}

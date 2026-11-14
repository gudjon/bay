//! G721 (a measurement, 2026-11-14: each `bay status` on a store of 6,300 changes ran about 40
//! ancestry checks and a dozen history queries over the same commits as the read before it). An
//! answer that reads only objects named for good is kept in the store, and a later process reads
//! it there instead of asking Git: the same listing, with fewer Git processes. A store with
//! replace refs keeps none, since there what a commit's history is can change.
//!
//! Guard to mutate: `src/facts.rs` `get`: answer `None` (every process asks Git again).

#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

const FIXED: [&str; 5] = ["log", "rev-list", "merge-base", "diff-tree", "ls-tree"];

/// A listing served by the last sweep, and how many fixed-query kinds of Git it started.
fn listing(test_box: &TestBox, repo: &std::path::Path, facts: &str, name: &str) -> (Value, usize) {
    let trace = test_box.path().join(format!("{name}.jsonl"));
    let (code, result) = result_json(
        test_box
            .bay(repo)
            .arg("status")
            .env("BAY_TEST_SAVED", "off")
            .env("BAY_TEST_SWEPT_S", "600")
            .env("BAY_TEST_FACTS", facts)
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{result}");
    let ran = std::fs::read_to_string(&trace)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| FIXED.contains(&line["sub"].as_str().unwrap_or_default()))
        .count();
    (result, ran)
}

fn store() -> (TestBox, std::path::PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let _ = test_box.start_and_edit(&repo);
    let _ = test_box.start_and_edit(&repo);
    // A sweep the reads below are served by.
    let (code, swept) = result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 0, "{swept}");
    support::sweeps_ended(&repo);
    (test_box, repo)
}

#[test]
fn a_later_listing_reads_the_answers_an_earlier_one_kept() {
    let (test_box, repo) = store();

    // GIVEN a listing that asked Git and kept its answers
    let (first, asked) = listing(&test_box, &repo, "on", "first");
    // WHEN the next process lists the store that did not move
    let (second, again) = listing(&test_box, &repo, "on", "second");
    // AND one that keeps nothing
    let (_, without) = listing(&test_box, &repo, "off", "without");

    // THEN it asks Git less than one that keeps nothing, and lists the same changes
    assert!(asked > 0, "the listing ran no fixed query");
    assert!(
        again < without,
        "fixed queries: {again} reading kept answers, {without} keeping none"
    );
    assert_eq!(second["detail"]["changes"], first["detail"]["changes"]);
}

#[test]
fn a_store_with_replace_refs_keeps_no_answer() {
    let (test_box, repo) = store();
    // GIVEN a replace ref: what a commit's history is can change
    let head = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    let tree = test_box.git_ok(&repo, &["rev-parse", "HEAD^{tree}"]);
    let other = test_box.git_ok(
        &repo,
        &["commit-tree", tree.trim(), "-m", "another history"],
    );
    test_box.git_ok(
        &repo,
        &[
            "update-ref",
            &format!("refs/replace/{}", head.trim()),
            other.trim(),
        ],
    );

    let _ = listing(&test_box, &repo, "on", "first");
    let (_, again) = listing(&test_box, &repo, "on", "second");
    let (_, without) = listing(&test_box, &repo, "off", "without");

    // THEN the next process asks Git as one that keeps nothing does
    assert_eq!(
        again, without,
        "a store with replace refs read kept answers"
    );
}

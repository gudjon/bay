//! G739 (a measurement, 2026-11-15: a listing of a clone of 6,200 changes ran about 50 one-ref
//! reads, 33 of them distinct, 20 of them change refs its own listing of the change refs had just
//! named). A status read keeps its ref reads once its sweep is done, until it ends: a ref read
//! again, or found absent, runs no Git, and a change ref the listing of the change refs named
//! runs none at all. A Git command that may move a ref still drops what was kept. Read from the
//! trace a test build writes (G171).
//!
//! Guard to mutate: in `src/change.rs` `Chain::read_all`, drop the call of `remember_refs`, and
//! the listing reads each described change's ref again.

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::{TestBox, unchecked_files};

/// The one-ref reads of change refs one listing of `repo` ran.
fn change_ref_reads(test_box: &TestBox, repo: &Path) -> usize {
    let trace = test_box.path().join("listing-trace.jsonl");
    let _ = std::fs::remove_file(&trace);
    let (code, status) = support::result_json(
        test_box
            .bay(repo)
            .args(["status"])
            .env("BAY_TEST_SAVED", "off")
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{status}");
    let text = std::fs::read_to_string(&trace).unwrap_or_default();
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref() && line["sub"] == "rev-parse")
        .filter(|line| line["args"].to_string().contains("refs/bay/changes/"))
        .count()
}

#[test]
fn a_listing_reads_no_change_ref_its_listing_of_them_named() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    // GIVEN three changes of the reader's
    for n in 0..3 {
        test_box.bay_ok(&repo, &["start", &format!("CARD-{n}"), "--paths", "src/"]);
    }

    // WHEN a listing describes them
    let reads = change_ref_reads(&test_box, &repo);

    // THEN it read no change ref one at a time
    assert_eq!(
        reads, 0,
        "the listing read {reads} change refs one at a time"
    );
}

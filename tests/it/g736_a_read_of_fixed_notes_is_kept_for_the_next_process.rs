//! G736 (a measurement, 2026-11-15: each listing of a clone of 6,200 changes read the evidence
//! notes of the same notes trees in two sections, a 4,100-line `cat-file --batch-check` and a
//! 984-line `cat-file --batch` among them, about 0.5 s). The evidence reads name each producer's
//! notes by its notes tree's id, never by its ref, so their answers cannot change: a batch read
//! of objects named for good is kept in the store's answers, also where it found a path missing
//! from a tree the store holds. A second listing makes none of the first's batch reads of notes
//! again. Read from the trace a test build writes (G171).
//!
//! Guard to mutate: in `src/evidence.rs` `notes_source`, always name the ref, and the second
//! listing reads the notes again.

// The host hook is a shell script, so this runs on Unix only.
#![cfg(unix)]

use crate::g606_a_status_judges_red_from_the_last_status::land;
use crate::support;

use std::path::Path;

use serde_json::Value;
use support::{TestBox, check_files, passing_result};

/// How many `cat-file --batch` reads one listing of `repo` spawned.
fn batch_reads(test_box: &TestBox, repo: &Path, name: &str) -> usize {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
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
        .filter(|line| Some(&line["pid"]) == pid.as_ref() && line["sub"] == "cat-file")
        .filter(|line| line["args"].to_string().contains("--batch"))
        .filter(|line| !line["input"].is_null())
        .count()
}

#[test]
fn a_second_listing_reads_no_notes_of_fixed_trees_again() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let files = check_files("true", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    // GIVEN landings whose checks left evidence notes
    for n in 0..2 {
        land(&test_box, &repo, &format!("src/n{n}.rs"));
    }

    // WHEN two listings read the store
    let first = batch_reads(&test_box, &repo, "first");
    let second = batch_reads(&test_box, &repo, "second");

    // THEN the second made fewer batch reads: the notes' were kept
    assert!(first > 0, "the first listing read no batch");
    assert!(
        second < first,
        "the second listing made {second} batch reads, the first {first}"
    );
}

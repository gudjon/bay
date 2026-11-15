//! G608 (a fleet finding, 2026-10-31: a status counted each required check's flaky landings
//! with one records read a landing, about 60 reads of the evidence refs on a window of 60). A
//! status reads the records of the whole window at once: it lists the evidence refs as often
//! with six landings as with two, and counts every landing.
//!
//! Guard to mutate, in `src/status.rs` `flakes`: read each landing's records alone, and a
//! status lists the evidence refs once more for each landing.

// The landing runs a shell check adapter, and the case puts a shell wrapper for `git` on PATH,
// so it runs on Unix only.
#![cfg(unix)]

use crate::g606_a_status_judges_red_from_the_last_status::land;
use crate::support;

use std::path::Path;

use bay::evidence::EVIDENCE_PREFIX;
use support::{TestBox, check_files, passing_result};

/// One `bay status`: how many times its own process listed the evidence refs, and the landings
/// its flake rows counted. A sweep is a process of its own, which runs when its window says, so
/// its listings are not the read's (read from the trace a test build writes, G171).
fn status(test_box: &TestBox, repo: &Path) -> (usize, u64) {
    let trace = test_box.path().join("status-trace.jsonl");
    let _ = std::fs::remove_file(&trace);
    let (code, status) = support::result_json(
        test_box
            .bay(repo)
            .args(["status"])
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{status}");
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&trace)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let lists = lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref() && line["sub"] == "for-each-ref")
        .filter(|line| line["args"].to_string().contains(EVIDENCE_PREFIX))
        .count();
    let landings = status["detail"]["flakes"][0]["merges"]
        .as_u64()
        .unwrap_or_else(|| panic!("no flake row: {status}"));
    (lists, landings)
}

#[test]
fn a_status_lists_the_evidence_refs_as_often_with_six_landings_as_with_two() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let files = check_files("true", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    for n in 0..2 {
        land(&test_box, &repo, &format!("src/first{n}.rs"));
    }

    let (two, read) = status(&test_box, &repo);
    assert!(read >= 2, "the flake rows counted {read} landings");

    for n in 0..4 {
        land(&test_box, &repo, &format!("src/more{n}.rs"));
    }
    let (six, more) = status(&test_box, &repo);
    assert_eq!(more, read + 4, "the flake rows lost a landing");
    // A read lists the refs once more when it fetches the evidence notes itself rather than
    // finding a sweep's fetch, which turns on timing, never on the landings: one listing a
    // landing would show four more here.
    assert!(
        six.abs_diff(two) <= 1,
        "a status listed the evidence refs {six} times with {more} landings, {two} with {read}"
    );
}

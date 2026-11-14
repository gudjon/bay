//! G719 (a measurement, 2026-11-14: one `bay status` started 77 `cat-file` processes, about a
//! third of its Git time). A read of objects named for good (an object id, with `~<n>`, `^<n>`
//! or `:<path>` after it) is answered by one `git cat-file --batch` the process keeps open, so it
//! starts no process of its own. A name that reads a ref is read as before. The listing is the
//! same either way.
//!
//! Guard to mutate: `src/git.rs` `Git::read_open`: answer `None` (every read starts a process).

#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

/// A listing, and how many `cat-file` processes it started.
fn listing(test_box: &TestBox, repo: &std::path::Path, reader: &str) -> (Value, usize) {
    let trace = test_box.path().join(format!("trace-{reader}.jsonl"));
    let _ = std::fs::remove_file(&trace);
    let (code, result) = result_json(
        test_box
            .bay(repo)
            .arg("status")
            .env("BAY_TEST_SAVED", "off")
            .env("BAY_TEST_OPEN_READER", reader)
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{result}");
    support::sweeps_ended(repo);
    let spawned = std::fs::read_to_string(&trace)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["sub"] == "cat-file")
        .count();
    (result, spawned)
}

#[test]
fn a_listing_reads_objects_named_for_good_with_no_process_of_their_own() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let (first, _) = test_box.start_and_edit(&repo);
    let (second, _) = test_box.start_and_edit(&repo);

    // GIVEN a listing that starts a process for each read
    let (before, spawned_before) = listing(&test_box, &repo, "off");
    // WHEN the process keeps one reader open
    let (after, spawned_after) = listing(&test_box, &repo, "on");

    // THEN it starts fewer `cat-file` processes, and lists the same changes
    assert!(
        spawned_after < spawned_before,
        "cat-file processes: {spawned_after} with the open reader, {spawned_before} without"
    );
    assert_eq!(after["detail"]["changes"], before["detail"]["changes"]);
    for id in [&first, &second] {
        assert!(
            after["detail"]["changes"].to_string().contains(id.as_str()),
            "{after}"
        );
    }
}

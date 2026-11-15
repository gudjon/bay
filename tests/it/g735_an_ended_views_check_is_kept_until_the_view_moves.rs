//! G735 (a measurement, 2026-11-15: on a clone of 6,200 changes a listing spent about 1 s
//! checking up to 20 of the reader's views for leftover work, most of them views of changes that
//! had ended, which nobody edits). The check of an ended change's view is kept in the store's
//! answers with a stamp of the view (its index, `HEAD` and root directory) and shown again, with
//! its age in `checked_s`, until the stamp moves or it is older than 10 minutes. A live change's
//! view is checked on every listing. Read from the trace a test build writes (G171).
//!
//! Guard to mutate: in `src/status.rs` `leftover_row`, never take a kept check, and the second
//! listing reads the view again.

// The views are made by shell-scripted fixtures, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::TestBox;

/// A listing of `repo`: the result and how many of bay's own `git diff` calls read `view`.
fn listing(test_box: &TestBox, repo: &Path, view: &Path, name: &str) -> (Value, usize) {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let (code, status) = support::result_json(
        test_box
            .bay(repo)
            .args(["status"])
            .env("BAY_TEST_SAVED", "off")
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{status}");
    let view = view.display().to_string();
    let text = std::fs::read_to_string(&trace).unwrap_or_default();
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let reads = lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref() && line["sub"] == "diff")
        .filter(|line| line["args"].to_string().contains(&view))
        .count();
    (status, reads)
}

fn row<'a>(status: &'a Value, id: &str) -> &'a Value {
    status["detail"]["leftovers"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["change"] == id))
        .unwrap_or_else(|| panic!("no leftover {id}: {status}"))
}

#[test]
fn an_ended_views_check_is_kept_until_the_view_moves() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    // GIVEN the view of a change that ended
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    test_box.bay_ok(&view, &["abandon"]);

    // WHEN two listings read it
    let (first, first_reads) = listing(&test_box, &repo, &view, "first");
    let (second, second_reads) = listing(&test_box, &repo, &view, "second");

    // THEN the first checked it and the second showed that check, with its age
    assert!(
        first_reads > 0,
        "the first listing did not check the view: {first}"
    );
    assert_eq!(second_reads, 0, "the second listing checked the view again");
    assert_eq!(
        row(&second, &id)["uncheckpointed"],
        row(&first, &id)["uncheckpointed"]
    );
    assert!(row(&second, &id)["checked_s"].is_u64(), "{second}");

    // AND a file made in the view moves its stamp: the next listing checks it again
    std::fs::write(view.join("left.txt"), "work\n").unwrap();
    let (third, third_reads) = listing(&test_box, &repo, &view, "third");
    assert!(
        third_reads > 0,
        "a moved view was not checked again: {third}"
    );
    assert_eq!(row(&third, &id)["uncheckpointed"], true, "{third}");
}

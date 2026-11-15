//! G747 (a measurement, 2026-11-15: after G743 a `bay start` on a store with 106 views still
//! read each view's change, its drafts and its end time, about 900 Git calls, before taking the
//! kept judgement). One listing of the change refs names every chain's head, and a view kept as
//! holding something newer at its change's head, with nothing in it moved, is passed over before
//! anything of its change is read. Read from the trace a test build writes (G171).
//!
//! Guard to mutate: in `src/start.rs` `release_ended`, drop the `kept_holding_newer` pass, and the
//! second start reads the change again.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

/// One `bay start` of `intent` in `repo`: how many of bay's own Git calls named change `id`.
fn reads_of(test_box: &TestBox, repo: &Path, id: &str, intent: &str) -> usize {
    let trace = test_box.path().join(format!("{intent}-trace.jsonl"));
    let (code, started) = result_json(
        test_box
            .bay(repo)
            .args(["start", intent, "--paths", "src/"])
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{started}");
    let lines: Vec<Value> = std::fs::read_to_string(&trace)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref())
        .filter(|line| {
            line["args"].to_string().contains(id) || line["input"].to_string().contains(id)
        })
        .count()
}

#[test]
fn a_kept_view_of_an_ended_change_is_passed_over_without_reading_its_change() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    // GIVEN a landed change whose view holds a later edit
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/one.rs"), "fn one() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    std::fs::write(view.join("src/a.rs"), "fn a() { later() }\n").unwrap();

    // WHEN a start judges it and a second start follows
    let first = reads_of(&test_box, &repo, &id, "CARD-2");
    let second = reads_of(&test_box, &repo, &id, "CARD-3");

    // THEN the first read its change, and the second read nothing of it
    assert!(first > 0, "the first start did not read the change");
    assert_eq!(
        second, 0,
        "the second start read the kept view's change again"
    );
}

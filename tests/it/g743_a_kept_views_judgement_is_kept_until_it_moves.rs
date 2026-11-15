//! G743 (a measurement, 2026-11-15: on a shared store with 91 views of ended changes, each
//! agent's `bay start` judged every one again, about 1,200 Git calls and a walk of each view's
//! tree for ignored directories, 42 s at the median, though nobody touched those views). A view
//! of an ended change judged to hold newer work is kept with that judgement, under its stamp (its
//! index, `HEAD` and root directory), and a later `bay start` judges it again only once the stamp
//! moves. Read from the trace a test build writes (G171).
//!
//! Guard to mutate: in `src/view.rs` `standing`, never take a kept judgement, and the second start
//! reads the view again.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

/// One `bay start` of `intent` in `repo`: how many of bay's own Git calls ran in `view`.
fn start_reads(test_box: &TestBox, repo: &Path, view: &Path, intent: &str) -> usize {
    let trace = test_box.path().join(format!("{intent}-trace.jsonl"));
    let (code, started) = result_json(
        test_box
            .bay(repo)
            .args(["start", intent, "--paths", "src/"])
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{started}");
    let view = view.display().to_string();
    let text = std::fs::read_to_string(&trace).unwrap_or_default();
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref())
        .filter(|line| line["args"].to_string().contains(&view))
        .count()
}

#[test]
fn a_kept_views_judgement_is_kept_until_the_view_moves() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    // GIVEN a landed change whose view holds a later edit
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/one.rs"), "fn one() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    std::fs::write(view.join("src/a.rs"), "fn a() { later() }\n").unwrap();

    // WHEN three later starts read the machine's views
    let first = start_reads(&test_box, &repo, &view, "CARD-2");
    let second = start_reads(&test_box, &repo, &view, "CARD-3");
    std::fs::write(view.join("notes.txt"), "more\n").unwrap();
    let third = start_reads(&test_box, &repo, &view, "CARD-4");

    // THEN the first judged it, the second took that judgement, and a moved view is judged again
    assert!(first > 0, "the first start did not judge the view");
    assert_eq!(second, 0, "the second start judged the view again");
    assert!(third > 0, "a moved view was not judged again");
    assert!(
        view.join("src/a.rs").exists(),
        "the kept view lost its work"
    );
}

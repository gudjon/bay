//! G85. An unchecked trunk commit is an unattributed landing (`DESIGN` § 10a).
//!
//! On a forge trunk, a merge or a push made outside bay did not land through bay. `bay status`
//! names each such first-parent commit under `detail.unattributed`, `landed_as` `merge` or
//! `outside`, and counts them; a merge also names the commits reached only through its other
//! parent, counted as `off_first_parent`. bay's own landing is never listed, and neither is the
//! commit that adopted bay. The fleet map shows the same, and `bay status --since` names each one as an
//! `unattributed_landing` event.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::{Value, json};
use support::{TestBox, result_json, unchecked_files};

/// Commits `file` in `dir` and returns the commit.
fn commit(test_box: &TestBox, dir: &Path, file: &str) -> String {
    std::fs::write(dir.join(file), format!("{file}\n")).unwrap();
    test_box.git_ok(dir, &["add", file]);
    test_box.git_ok(dir, &["commit", "-q", "-m", file]);
    test_box.git_ok(dir, &["rev-parse", "HEAD"])
}

/// The commits `unattributed` lists, newest first.
fn listed(unattributed: &Value) -> Vec<&str> {
    unattributed["merges"]
        .as_array()
        .unwrap_or_else(|| panic!("{unattributed}"))
        .iter()
        .map(|landing| landing["commit"].as_str().unwrap())
        .collect()
}

#[test]
fn a_merge_and_a_push_outside_bay_are_named_and_counted_and_bays_landing_is_not() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let adopted =
        test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());

    // The commit that adopted bay is not an unattributed landing.
    let status = test_box.bay_ok(&repo, &["status"]);
    let before = &status["detail"]["unattributed"];
    assert_eq!(before["count"], 0, "{adopted} adopted bay: {status}");
    let cursor = test_box.bay_ok(&repo, &["status", "--since"])["detail"]["cursor"]
        .as_str()
        .unwrap()
        .to_owned();

    // A change lands through bay.
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "merged", "{result}");
    test_box.git_ok(&repo, &["pull", "-q", "--ff-only", "origin", "main"]);
    let published = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);

    // A commit pushed straight to the trunk host, outside bay.
    let outside = commit(&test_box, &repo, "outside.txt");
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);
    // A merge of a side branch, made outside bay.
    test_box.git_ok(&repo, &["switch", "-q", "-c", "side"]);
    let side = commit(&test_box, &repo, "side.txt");
    test_box.git_ok(&repo, &["switch", "-q", "main"]);
    test_box.git_ok(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    let merge = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);

    let status = test_box.bay_ok(&repo, &["status"]);
    let unattributed = &status["detail"]["unattributed"];
    assert_eq!(
        listed(unattributed),
        [merge.as_str(), outside.as_str()],
        "exactly the merge and the outside push, newest first, never bay's landing {published}: {status}"
    );
    assert_eq!(unattributed["count"], 2, "{unattributed}");
    assert_eq!(unattributed["window"], 50, "{unattributed}");
    assert_eq!(unattributed["off_first_parent"], 1, "{unattributed}");

    let merged = &unattributed["merges"][0];
    assert_eq!(merged["merged_as"], "merge", "{merged}");
    assert_eq!(merged["base"], outside.as_str(), "{merged}");
    assert_eq!(merged["merged"], json!([side]), "{merged}");
    assert_eq!(merged["merged_count"], 1, "{merged}");
    let pushed = &unattributed["merges"][1];
    assert_eq!(pushed["merged_as"], "outside", "{pushed}");
    assert_eq!(pushed["base"], published.as_str(), "{pushed}");
    assert_eq!(pushed["merged_count"], 0, "{pushed}");

    // The fleet map reads the same from the cache `bay status` fetched.
    let fleet = test_box.bay_ok(&repo, &["status", "--all"]);
    assert_eq!(fleet["detail"]["unattributed"], *unattributed, "{fleet}");

    // `bay status --since` names each as an event, and never bay's landing.
    let watched = test_box.bay_ok(&repo, &["status", "--since", &cursor]);
    let events: Vec<&Value> = watched["detail"]["events"]
        .as_array()
        .unwrap_or_else(|| panic!("{watched}"))
        .iter()
        .filter(|event| event["kind"] == "outside_merge")
        .collect();
    let named: Vec<(&str, &str)> = events
        .iter()
        .map(|event| {
            (
                event["commit"].as_str().unwrap(),
                event["merged_as"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        named,
        [(outside.as_str(), "outside"), (merge.as_str(), "merge")],
        "oldest first, as the landings: {watched}"
    );
}

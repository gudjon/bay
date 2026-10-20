//! G41. Leftovers carry state, age, and uncheckpointed edits.
//!
//! `bay status` lists each leftover with `state`, `age`, and `uncheckpointed`. The result has
//! no pull-request state and no chat activity.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::TestBox;

fn leftover<'a>(status: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    status["detail"]["leftovers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|leftover| leftover["change"] == id)
        .unwrap_or_else(|| panic!("no leftover {id}: {status}"))
}

#[test]
fn each_leftover_reports_state_age_and_uncheckpointed_edits() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n"), ("docs/b.md", "b\n")]);
    let edited = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let untouched = test_box.bay_ok(&repo, &["start", "CARD-2", "--paths", "src/"]);
    let edited_id = edited["change"].as_str().unwrap().to_owned();
    let untouched_id = untouched["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(edited["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/a.rs"), "fn a() { edited }\n").unwrap();

    let status = test_box.bay_ok(&repo, &["status"]);
    let one = leftover(&status, &edited_id);
    assert_eq!(one["state"], "open");
    assert!(one["age_s"].is_u64(), "{one}");
    assert_eq!(one["uncheckpointed"], true);
    let two = leftover(&status, &untouched_id);
    assert_eq!(
        two["uncheckpointed"], false,
        "absent sparse files are not edits"
    );

    // A checkpoint records the edit.
    test_box.bay_ok(&repo, &["start", "--resume", &edited_id]);
    let status = test_box.bay_ok(&repo, &["status"]);
    assert_eq!(leftover(&status, &edited_id)["uncheckpointed"], false);

    // An untracked file alone is an uncheckpointed edit too, until a checkpoint records it.
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let status = test_box.bay_ok(&repo, &["status"]);
    assert_eq!(leftover(&status, &edited_id)["uncheckpointed"], true);
    test_box.bay_ok(&repo, &["start", "--resume", &edited_id]);
    let status = test_box.bay_ok(&repo, &["status"]);
    assert_eq!(leftover(&status, &edited_id)["uncheckpointed"], false);

    let text = status.to_string();
    assert!(
        !text.contains("pull_request") && !text.contains("chat"),
        "{text}"
    );
}

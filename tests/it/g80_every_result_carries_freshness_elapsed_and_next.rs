//! G80: every result says which trunk it reflects (`INTERFACES` § 1).
//!
//! `start`, `land` and `undo` fetch trunk, and their results carry `freshness`: the trunk tip
//! the answer reflects and when this command fetched it, as `status` already does.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

fn assert_fresh(result: &serde_json::Value, trunk: &str) {
    assert_eq!(result["freshness"]["main"], trunk, "{result}");
    let at = result["freshness"]["fetched_at"]
        .as_str()
        .unwrap_or_default();
    assert!(
        at.len() == 20 && at.ends_with('Z') && at.as_bytes()[10] == b'T',
        "fetched by this command, at an RFC 3339 time: {result}"
    );
}

#[test]
fn start_land_and_undo_carry_freshness() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let base = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    assert_fresh(&started, &base);

    let id = started["change"].as_str().unwrap().to_owned();
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let commit = test_box.git_bare(&remote, &["rev-parse", "main"]);
    assert_eq!(landed["detail"]["commit"], commit.as_str());
    assert_fresh(&landed, &commit);

    let (_, undone) = result_json(test_box.bay(&repo).args(["undo", &id]));
    assert!(undone["freshness"].is_object(), "{undone}");
}

/// An asynchronous land, and `status` of its operation, say how long it has been under way,
/// which evidence it has so far, and the state it moves to next.
#[test]
fn a_queued_land_says_elapsed_evidence_and_state() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");
    assert!(queued["elapsed_s"].is_u64(), "{queued}");
    assert_eq!(queued["evidence"], serde_json::json!([]), "{queued}");
    // 0.3.0: `state` already says where it is, so the result names no `next` and no `published`.
    assert!(queued.get("next").is_none(), "{queued}");
    assert!(queued.get("published").is_none(), "{queued}");
    let state = queued["state"].as_str().unwrap();
    let next = match state {
        "submitted" => "admitted",
        "admitted" => "running",
        other => panic!("a queued land in state {other}: {queued}"),
    };

    let status = test_box.bay_ok(&repo, &["status", &id]);
    let operation = &status["detail"]["changes"][0]["operation"];
    assert!(operation["elapsed_s"].is_u64(), "{operation}");
    assert_eq!(operation["next"], next, "{operation}");
}

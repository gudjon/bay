//! G13. A submission outlives its session.
//!
//! Disconnect while queued, while running, after a receipt is stored, and after publication
//! before the acknowledgement: reconnecting finds the true state; no duplicate entry or
//! publication.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use support::{TestBox, check_files, passing_result, result_json};

fn change(status: &serde_json::Value) -> &serde_json::Value {
    &status["detail"]["changes"][0]
}

fn operations(test_box: &TestBox, executor: &std::path::Path, id: &str) -> usize {
    let record = test_box.git_ok(
        executor,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    record["operations"].as_array().map_or(0, Vec::len)
}

/// A submitted change and an executor clone. Returns (box, repo, executor, remote, id).
fn submitted(
    result: &str,
) -> (
    TestBox,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    String,
) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", result, 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{queued}");
    let executor = test_box.clone_of(&remote, "executor");
    (test_box, repo, executor, remote, id)
}

#[test]
fn disconnected_while_queued() {
    let (test_box, repo, executor, remote, id) = submitted(&passing_result(1));
    let status = test_box.bay_ok(&repo, &["status", &id]);
    assert_eq!(
        change(&status)["operation"]["state"],
        "submitted",
        "{status}"
    );
    assert_eq!(change(&status)["operation"]["published"], false);
    test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    let status = test_box.bay_ok(&repo, &["status", &id]);
    assert_eq!(change(&status)["state"], "landed");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    assert_eq!(operations(&test_box, &executor, &id), 1);
}

#[test]
fn disconnected_while_running() {
    let (test_box, repo, executor, remote, id) = submitted(&passing_result(1));
    let killed = test_box
        .bay(&executor)
        .env("BAY_TEST_KILL_AT", "after-commit")
        .args(["executor", "run", "--once"])
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), Some(137));
    let status = test_box.bay_ok(&repo, &["status", &id]);
    assert_ne!(change(&status)["state"], "landed", "{status}");
    assert_eq!(change(&status)["operation"]["published"], false);
    test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    let status = test_box.bay_ok(&repo, &["status", &id]);
    assert_eq!(change(&status)["state"], "landed");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    assert_eq!(operations(&test_box, &executor, &id), 1);
}

#[test]
fn disconnected_after_a_receipt_is_stored() {
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let (test_box, repo, executor, remote, id) = submitted(&failing);
    test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let op = &change(&status)["operation"];
    assert_eq!(op["outcome"], "CHECK_FAILED", "{status}");
    assert_eq!(
        change(&status)["receipts"].as_array().unwrap().len(),
        1,
        "{status}"
    );
    // Reconnecting again adds nothing.
    test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    let status = test_box.bay_ok(&repo, &["status", &id]);
    assert_eq!(
        change(&status)["receipts"].as_array().unwrap().len(),
        1,
        "{status}"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());
    assert_eq!(operations(&test_box, &executor, &id), 1);
}

#[test]
fn disconnected_after_publication_before_the_acknowledgement() {
    let (test_box, repo, executor, remote, id) = submitted(&passing_result(1));
    let killed = test_box
        .bay(&executor)
        .env("BAY_TEST_KILL_AT", "after-publish")
        .args(["executor", "run", "--once"])
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), Some(137));
    let status = test_box.bay_ok(&repo, &["status", &id]);
    assert_eq!(change(&status)["state"], "landed", "{status}");
    let view = repo.join(".git/bay/views").join(&id);
    let (code, again) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["decision"], "landed");
    test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    assert_eq!(operations(&test_box, &executor, &id), 1);
}

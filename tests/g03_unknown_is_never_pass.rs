//! G3. Unknown is never pass.
//!
//! A required check is missing, crashes, times out or collects nothing: land stays blocked
//! with `CHECK_UNKNOWN`, and trunk does not move.

mod support;

use support::{TestBox, check_files, passing_result, result_json};

fn blocked_with(
    before: &str,
    result: &str,
    expected_minimum: u32,
    timeout_s: u32,
) -> serde_json::Value {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let trunk = test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(before, result, expected_minimum, timeout_s),
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        trunk,
        "trunk moved"
    );
    result
}

#[test]
fn a_check_that_collects_nothing_is_unknown() {
    let result = blocked_with("true", &passing_result(0), 1, 60);
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
    assert_eq!(result["allowed"], serde_json::json!(["wait", "repair"]));
}

#[test]
fn fewer_than_expected_minimum_is_unknown() {
    let result = blocked_with("true", &passing_result(2), 3, 60);
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
    assert!(
        result.to_string().contains("expected_minimum 3"),
        "{result}"
    );
}

#[test]
fn a_check_that_crashes_is_unknown() {
    let result = blocked_with("exit 3", "true", 1, 60);
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
}

#[test]
fn a_check_that_times_out_is_unknown() {
    let result = blocked_with("sleep 5", &passing_result(1), 1, 1);
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
    assert!(result.to_string().contains("timed out"), "{result}");
}

#[test]
fn an_exit_code_alone_is_not_evidence() {
    let result = blocked_with("echo all good; exit 0", "true", 1, 60);
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
}

#[test]
fn a_result_for_another_commit_is_rejected_whole() {
    let result = blocked_with(
        "commit=0000000000000000000000000000000000000000",
        &passing_result(1),
        1,
        60,
    );
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
    assert!(result.to_string().contains("different commit"), "{result}");
}

#[test]
fn a_product_failure_is_check_failed() {
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let result = blocked_with("true", &failing, 1, 60);
    assert_eq!(result["reason"], "CHECK_FAILED");
}

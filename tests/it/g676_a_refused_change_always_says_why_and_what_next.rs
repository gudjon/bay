//! G676 (a lint change sat `blocked` 262 s with reason, allowed and next step all empty, and no
//! session was woken: a wake needs a reason). Every refusal of a policy edit in the check step
//! carries `POLICY_INVALID`, its next step and its retry rule, on the result and on the operation,
//! so `bay status` and the wake say it. A Git fault there is a fault, retried, never a silent block.
//!
//! Guard to mutate, in `src/publish.rs` `check_candidate`: `refuse` building a plain `failed`
//! result again (the outcome is empty).

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_change_that_removes_the_policy_says_why_and_what_next() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("", &passing_result(2), 2, 60),
    );
    let started = test_box.bay_ok(&repo, &["start", "no rules", "--paths", ".bay/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::remove_file(view.join(".bay/policy.toml")).unwrap();

    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
    assert!(refused["next_step"].is_string(), "{refused}");
    assert!(refused["retry_when"].is_string(), "{refused}");
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["outcome"], "POLICY_INVALID", "{status}");
    assert!(op["next_step"].is_string(), "{status}");
}

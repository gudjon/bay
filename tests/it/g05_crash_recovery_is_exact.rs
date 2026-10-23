//! G5. Crash recovery is exact.
//!
//! Kill bay at each step of `DESIGN` § 6 (after freeze, after commit, after persist,
//! after publish): `status` reports the true state; resuming finishes.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

fn kill_then_resume(point: &str, expect_state: &str, expect_op: &str) {
    let test_box = TestBox::new();
    let repo = test_box.repo("repo", "sha256", "reftable");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);

    let killed = test_box
        .bay(&view)
        .env("BAY_TEST_KILL_AT", point)
        .args(["merge", "--wait"])
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), Some(137), "{point}");

    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    assert_eq!(change["state"], expect_state, "{point}: {status}");
    assert_eq!(change["operation"]["state"], expect_op, "{point}: {status}");
    assert_eq!(
        change["operation"]["is_merged"],
        expect_state == "merged",
        "{point}: {status}"
    );

    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{point}: {result}");
    assert_eq!(result["decision"], "merged", "{point}");
    // Recovery continues the operation the crash interrupted; it never mints a second one.
    assert_eq!(result["op_id"], format!("{id}.1"), "{point}: {result}");
    assert_eq!(
        test_box.trunk_change_ids(&remote),
        vec![id.clone()],
        "{point}"
    );
    let status = test_box.bay_ok(&repo, &["status", &id]);
    assert_eq!(status["detail"]["changes"][0]["state"], "merged", "{point}");
}

#[test]
fn killed_after_freeze() {
    kill_then_resume("after-freeze", "ready", "submitted");
}

#[test]
fn killed_after_commit() {
    kill_then_resume("after-commit", "ready", "running");
}

#[test]
fn killed_after_persist() {
    kill_then_resume("after-persist", "ready", "checked");
}

#[test]
fn killed_after_publish() {
    kill_then_resume("after-publish", "merged", "merged");
}

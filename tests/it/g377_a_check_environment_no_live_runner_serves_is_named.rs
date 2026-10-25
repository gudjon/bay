//! G377 (a fleet finding, 2026-10-25: a policy merged with checks in one environment while the
//! only runner declared another, and those checks answered `unavailable` with nothing said). A
//! check environment no live runner serves is named.
//!
//! With operations queued and a live runner, `bay status` warns `unserved_environment`, naming
//! each environment a check of the policy on main runs in that no live runner declares, the
//! checks in it, and what the runners serve. A live runner that declares nothing serves every
//! environment, and a warning is never a refusal.
//!
//! Guard to mutate: `src/status.rs`: drop the `unserved_environments` chain (no warning).

use crate::support;

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

fn unserved(result: &Value) -> Option<&Value> {
    result["detail"]["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == "unserved_environment")
}

#[test]
fn queued_work_names_the_environment_no_live_runner_serves() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let runner = test_box.clone_of(&remote, "runner");
    test_box.git_ok(
        &runner,
        &["config", "--add", "bay.checkEnvironment", "elsewhere"],
    );
    let (code, ran) = result_json(test_box.bay(&runner).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{ran}");

    let (_, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let (code, status) = result_json(test_box.bay(&view).arg("status"));
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["decision"], "ok", "{status}");
    let warning = unserved(&status).unwrap_or_else(|| panic!("no warning: {status}"));
    assert_eq!(
        warning["environments"],
        serde_json::json!(["local"]),
        "{status}"
    );
    assert_eq!(warning["checks"][0]["environment"], "local", "{status}");
    assert_eq!(
        warning["served"],
        serde_json::json!(["elsewhere"]),
        "{status}"
    );

    // A runner that declares nothing serves every environment: nothing to warn of.
    let other = TestBox::new();
    let repo = other.repo_any("repo");
    let remote = other.path().join("repo-remote.git");
    other.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let runner = other.clone_of(&remote, "runner");
    let (code, ran) = result_json(other.bay(&runner).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{ran}");
    let (_, view) = other.start_and_edit(&repo);
    let (code, queued) = result_json(other.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let (_, status) = result_json(other.bay(&view).arg("status"));
    assert!(unserved(&status).is_none(), "{status}");
}

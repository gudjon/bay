//! G68. An executor below its floor does not run.
//!
//! `GIT-FEATURES` § 1 gives a box in the executor role Git 2.50. `bay runner run` on an older
//! Git refuses with `TOOL_BELOW_FLOOR` before it fetches or claims anything; at the floor it runs.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn an_executor_on_git_2_49_refuses_and_claims_nothing() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let executor = test_box.clone_of(&remote, "executor");
    let refs_before = test_box.git_bare(&remote, &["for-each-ref"]);

    test_box.fake_git_version("2.49.0");
    let (code, result) = result_json(test_box.bay(&executor).args(["runner", "run", "--once"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["decision"], "blocked", "{result}");
    assert_eq!(result["reason"], "TOOL_BELOW_FLOOR", "{result}");
    assert_eq!(result["detail"]["tool"], "git", "{result}");
    assert_eq!(result["detail"]["role"], "executor", "{result}");
    assert_eq!(result["detail"]["version"], "2.49.0", "{result}");
    assert_eq!(result["detail"]["floor"], "2.50", "{result}");
    assert_eq!(test_box.git_bare(&remote, &["for-each-ref"]), refs_before);
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());

    test_box.fake_git_version("2.50.0");
    test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

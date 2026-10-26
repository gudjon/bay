//! G392: while the policy on main itself breaks a validity rule, a change that does not touch
//! the policy is not refused. It waits with `POLICY_INVALID`, holds no queue place, and merges
//! once main's policy is fixed, with no step from its author (a fleet finding, 2026-10-26: an
//! invalid policy merged to main by hand refused every head for half an hour).

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

const POLICY: &str = ".bay/policy.toml";

/// Commits `policy` as main's `.bay/policy.toml` from the clone `outside`, as a person would.
fn push_policy(test_box: &TestBox, outside: &std::path::Path, policy: &str) {
    test_box.git_ok(outside, &["pull", "-q", "--ff-only", "origin", "main"]);
    std::fs::write(outside.join(POLICY), policy).unwrap();
    test_box.git_ok(outside, &["commit", "-q", "-am", "policy"]);
    test_box.git_ok(outside, &["push", "-q", "origin", "HEAD:main"]);
}

#[test]
fn a_change_waits_for_mains_policy_to_be_fixed_and_then_merges() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = check_files("true", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let valid = files
        .iter()
        .find(|(path, _)| path == POLICY)
        .map(|(_, text)| text.clone())
        .unwrap();
    let outside = test_box.clone_of(&remote, "outside");
    push_policy(
        &test_box,
        &outside,
        &valid.replacen("revision = 1\n", "revision = 2\nunchecked = true\n", 1),
    );

    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    let one = &ran["detail"]["ran"][0];
    assert_eq!(
        one["decision"], "queued",
        "a change main's mistake cannot judge waits: {ran}"
    );
    assert_eq!(one["reason"], "POLICY_INVALID", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());

    push_policy(
        &test_box,
        &outside,
        &valid.replacen("revision = 1\n", "revision = 3\n", 1),
    );
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id], "{ran}");
}

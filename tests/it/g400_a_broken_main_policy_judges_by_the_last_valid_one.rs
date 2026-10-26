//! G400 (B2, owner ruling 2026-10-26; a fleet finding: an invalid policy merged to main by hand
//! refused every head for a quarter of an hour, twice). While the newest policy on main breaks
//! a validity rule, a change is judged by the last valid policy on main's first-parent line, and
//! its result names that policy and why main's is invalid.
//!
//! Guard to mutate: `src/publish.rs` `last_valid_policy` returning `None`.

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
fn a_change_merges_by_the_last_valid_policy_and_says_so() {
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
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let used = &landed["detail"]["policy_used"];
    assert_eq!(used["revision"], 1, "{landed}");
    assert!(
        used["why"]
            .as_str()
            .is_some_and(|why| why.contains("unchecked")),
        "{landed}"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

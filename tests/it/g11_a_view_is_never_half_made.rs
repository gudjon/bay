//! G11. A view is never half-made.
//!
//! Kill `start` mid-creation: no candidate can be built from the view; `--resume` completes it.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_view_killed_mid_creation_builds_no_candidate_until_resumed() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let trunk = test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/b.md", "b\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );

    let killed = test_box
        .bay(&repo)
        .env("BAY_TEST_KILL_AT", "start-mid-creation")
        .args(["start", "CARD-1", "--paths", "src/"])
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), Some(137));
    let id = test_box
        .git_ok(
            &repo,
            &[
                "for-each-ref",
                "--format=%(refname:lstrip=3)",
                "refs/bay/changes/",
            ],
        )
        .trim()
        .to_owned();
    let view = repo.join(".git/bay/views").join(&id);
    assert!(view.exists(), "the half-made view is on disk");
    assert!(!view.join("src/a.rs").exists(), "and it is empty");

    // An empty view would read as "delete everything"; bay builds nothing from it.
    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .args(["land", "--wait", "--change", &id]),
    );
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "WORKSPACE_INCOMPLETE");
    assert_eq!(result["allowed"], serde_json::json!(["refresh"]));
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
    let chain = test_box.git_ok(
        &repo,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    assert!(
        !chain.contains("operations"),
        "no operation was submitted: {chain}"
    );

    // --resume completes the view; then it lands like any other.
    let resumed = test_box.bay_ok(&repo, &["start", "--resume", &id]);
    let view = PathBuf::from(resumed["detail"]["view"].as_str().unwrap());
    assert!(view.join("src/a.rs").is_file());
    assert!(!view.join("docs/b.md").exists());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    let (code, result) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "landed");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:docs/b.md"]),
        "b",
        "nothing was deleted"
    );
}

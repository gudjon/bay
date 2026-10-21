//! G261. A submission the executor never took never lands (`INTERFACES` § 3a).
//!
//! With `[executor] command` set, `bay land` persists the request, then asks that executor to
//! `submit` it. When the command cannot be started, or it refuses, the land fails naming the
//! executor command, never `git`, and the operation ends `failed` with a stated outcome, so no
//! executor pass lands it later: a land that answered `failed` must not merge.
//!
//! The guard to mutate: the failed state `admit` in `src/publish.rs` writes when the submission
//! does not succeed (leave the operation as it was).

// Each case runs shell scripts (the executor adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

/// A repository whose `[executor] command` is `scripts/executor`, with `script` there when
/// given; `bay land` on one change, then one executor pass. Returns the land result, the
/// operation `bay status` shows, and the change ids the pass put on trunk.
fn land_with(script: Option<&str>) -> (serde_json::Value, serde_json::Value, Vec<String>) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    if let Some(script) = script {
        files.push(("scripts/executor".to_owned(), script.to_owned()));
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[executor]\ncommand = \"scripts/executor\"\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    if script.is_some() {
        std::fs::set_permissions(
            repo.join("scripts/executor"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
    }
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, failed) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 20, "{failed}");
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let op = status["detail"]["changes"][0]["operation"].clone();
    let executor = test_box.clone_of(&remote, "executor");
    test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    (failed, op, test_box.trunk_change_ids(&remote))
}

#[test]
fn an_executor_command_that_cannot_start_fails_the_land_by_name_and_nothing_lands() {
    let (failed, op, landed) = land_with(None);
    let text = failed.to_string();
    assert!(
        text.contains("scripts/executor"),
        "names the command: {failed}"
    );
    assert!(!text.contains("failed to run git"), "{failed}");
    assert_eq!(op["state"], "failed", "{op}");
    assert_eq!(op["outcome"], "submission_failed", "{op}");
    assert!(landed.is_empty(), "a failed land never merges: {landed:?}");
}

#[test]
fn an_executor_that_refuses_the_submission_fails_the_land_and_nothing_lands() {
    let (failed, op, landed) = land_with(Some("#!/bin/sh\necho 'queue closed' >&2\nexit 3\n"));
    assert!(
        failed.to_string().contains("queue closed"),
        "carries the executor's reason: {failed}"
    );
    assert_eq!(op["state"], "failed", "{op}");
    assert_eq!(op["outcome"], "submission_refused", "{op}");
    assert!(landed.is_empty(), "a refused land never merges: {landed:?}");
}

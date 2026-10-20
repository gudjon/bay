//! Item 9a demonstration: an agent submits, disconnects, and returns to a published change or
//! a precise repair request (`ROADMAP` § 2, increment 1).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn the_agent_leaves_and_the_executor_publishes() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);

    // land returns at once: queued, not published, with its operation id.
    let (code, queued) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued");
    assert_eq!(queued["published"], false);
    assert_eq!(queued["op_id"], format!("{id}.1"));
    assert_eq!(queued["waiting_for"], "executor");
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());

    // The agent and its machine are gone.
    std::fs::remove_dir_all(&repo).unwrap();

    // An executor on another clone finishes the operation.
    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "landed", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/new.rs"]),
        "fn new() {}"
    );

    // The agent returns on a fresh clone and finds the published change.
    let back = test_box.clone_of(&remote, "back");
    let status = test_box.bay_ok(&back, &["status", &id]);
    assert_eq!(
        status["detail"]["changes"][0]["state"], "landed",
        "{status}"
    );

    // A second pass has nothing to do.
    let again = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(again["detail"]["ran"], serde_json::json!([]));
}

#[test]
fn the_agent_leaves_and_returns_to_a_precise_repair_request() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &failing, 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);
    std::fs::remove_dir_all(&repo).unwrap();

    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["reason"], "CHECK_FAILED", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&remote), Vec::<String>::new());

    let back = test_box.clone_of(&remote, "back");
    let status = test_box.bay_ok(&back, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["state"], "blocked", "{status}");
    assert_eq!(op["outcome"], "CHECK_FAILED");
    assert_eq!(op["allowed"], serde_json::json!(["repair"]));
    assert_eq!(op["retry_when"], "candidate_changed");

    // A blocked operation waits for repair; the executor does not loop on it.
    let again = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(again["detail"]["ran"], serde_json::json!([]));
}

#[test]
fn a_configured_executor_admits_the_submission_with_a_job_handle() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        "scripts/executor".to_owned(),
        "#!/bin/sh\ncat > /dev/null\necho '{\"job\": \"ci-42\"}'\n".to_owned(),
    ));
    files.push((
        ".bay/config.toml".to_owned(),
        "[executor]\ncommand = \"scripts/executor\"\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    std::fs::set_permissions(
        repo.join("scripts/executor"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["state"], "admitted");
    let record = test_box.git_ok(
        &repo,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    assert!(record.contains("\"job\": \"ci-42\""), "{record}");
}

//! G14. A caller's timeout never destroys work.
//!
//! The observation timeout expires mid-run: the run continues. The submission timeout and the
//! execution budget each end the operation with a stated outcome on their own monotonic
//! clock; finished check units keep their receipts; a one-off `status` reports a suspected
//! lease expiry and never claims.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn the_observation_timeout_detaches_the_caller_and_the_run_continues() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        // The check outlasts the caller's bound by far, so a caller that waited for the run
        // cannot pass on a slow machine, and one that detached cannot fail on one.
        &check_files("sleep 15", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);

    let started = Instant::now();
    let (code, detached) = result_json(test_box.bay(&view).args(["merge", "--wait=1"]));
    assert_eq!(code, 0, "{detached}");
    assert_eq!(detached["decision"], "queued");
    assert_eq!(detached["state"], "running");
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(10),
        "the caller waited for the run: {waited:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if test_box.trunk_change_ids(&remote) == vec![id.clone()] {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the run did not finish after the caller left"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
    let status = test_box.bay_ok(&repo, &["status", &id]);
    assert_eq!(status["detail"]["changes"][0]["state"], "merged");
}

#[test]
fn the_execution_budget_ends_the_check_with_a_stated_outcome() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("sleep 5", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nexecution_budget_s = 1\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, blocked) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_UNKNOWN");
    assert!(
        blocked.to_string().contains("execution budget"),
        "{blocked}"
    );
}

#[test]
fn the_submission_timeout_ends_the_operation_with_a_stated_outcome() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        "scripts/executor".to_owned(),
        "#!/bin/sh\nsleep 5\necho '{\"job\": \"late\"}'\n".to_owned(),
    ));
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\ncommand = \"scripts/executor\"\nsubmission_timeout_s = 1\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    std::fs::set_permissions(
        repo.join("scripts/executor"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, failed) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 20, "{failed}");
    assert!(
        failed.to_string().contains("submission timeout"),
        "{failed}"
    );
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["state"], "failed", "{status}");
    assert_eq!(op["outcome"], "submission_timeout");
}

#[test]
fn a_one_off_status_reports_a_suspected_lease_expiry_and_never_claims() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let started = test_box.path().join("check-started");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(
            &format!(
                "{}sleep 30",
                test_box.relay(&format!("touch '{}'", started.display()))
            ),
            &passing_result(1),
            1,
            60,
        ),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0);
    let executor = test_box.clone_of(&remote, "executor");
    let mut owner = test_box
        .bay_process(&executor)
        .args(["runner", "run", "--once"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    while !started.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(100));
    }
    owner.kill().unwrap();
    let _ = owner.wait();
    std::thread::sleep(Duration::from_secs(3));

    let chain = format!("refs/bay/changes/{id}");
    let before = test_box.git_bare(&remote, &["rev-parse", &chain]);
    let status = test_box.bay_ok(&repo, &["status", &id]);
    assert_eq!(
        status["detail"]["changes"][0]["operation"]["lease"], "suspected expired",
        "{status}"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", &chain]),
        before,
        "status never claims"
    );
}

#[test]
fn finished_check_units_keep_their_receipts_after_the_caller_leaves() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let base = check_files("[ \"$1\" = slow ] && sleep 2", &passing_result(1), 1, 60);
    let script = base
        .iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1
        .clone();
    let policy = "revision = 1\n\n[[check]]\nid = \"fast\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check fast\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n\n[[check]]\nid = \"slow\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check slow\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
    let files = vec![
        ("scripts/bay-check".to_owned(), script),
        (".bay/policy.toml".to_owned(), policy.to_owned()),
    ];
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, detached) = result_json(test_box.bay(&view).args(["merge", "--wait=1"]));
    assert_eq!(code, 0, "{detached}");
    assert_eq!(detached["state"], "running");
    let deadline = Instant::now() + Duration::from_secs(60);
    while test_box.trunk_change_ids(&remote) != vec![id.clone()] {
        assert!(Instant::now() < deadline, "the run did not finish");
        std::thread::sleep(Duration::from_millis(250));
    }
    let tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    let notes = test_box.git_bare(
        &remote,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/local",
            "show",
            &tree,
        ],
    );
    let records: Vec<serde_json::Value> = serde_json::from_str(&notes).unwrap();
    let checks: Vec<&str> = records
        .iter()
        .map(|record| record["check"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(checks, ["fast", "slow"]);
    assert!(records.iter().all(|record| record["verdict"] == "pass"));
}

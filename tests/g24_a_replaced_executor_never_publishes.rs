//! G24. A replaced executor never publishes.
//!
//! Executor A pauses after its check passes; executor B takes over with a higher epoch and
//! lands the change; A resumes and does not push: it reports the change already landed. The
//! change's `Change-Id` appears on trunk once.

mod support;

use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_paused_executor_resumes_after_a_takeover_and_never_publishes() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);

    let a = test_box.clone_of(&remote, "executor-a");
    let b = test_box.clone_of(&remote, "executor-b");
    for (clone, name) in [(&a, "exec-a"), (&b, "exec-b")] {
        test_box.git_ok(clone, &["config", "bay.executorName", name]);
        test_box.git_ok(
            clone,
            &["config", "bay.producerName", &format!("{name}-checks")],
        );
    }
    let release = test_box.path().join("release-a");
    let paused = test_box.path().join("release-a.paused");
    let child = test_box
        .bay_process(&a)
        .env("BAY_TEST_PAUSE_AT", "after-persist")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["--json", "executor", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !paused.exists() {
        assert!(
            Instant::now() < deadline,
            "executor A never reached its pause"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // B takes over with a higher epoch and lands the change on its own candidate.
    let ran = test_box.bay_ok(&b, &["executor", "run", "--once"]);
    let landed = &ran["detail"]["ran"][0];
    assert_eq!(landed["decision"], "landed", "{ran}");
    assert_ne!(landed["detail"]["existing"], true);
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let epoch = test_box.git_bare(
        &remote,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Bay-Epoch,valueonly)",
            "main",
        ],
    );
    assert_eq!(epoch.trim(), "2");

    // A resumes: its epoch is stale, so it reports the publication and pushes nothing.
    std::fs::write(&release, "go").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let a_result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let a_ran = &a_result["detail"]["ran"][0];
    assert!(
        a_ran["detail"]["existing"] == true || a_ran["detail"]["superseded"] == true,
        "{a_result}"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        tip,
        "A pushed nothing"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

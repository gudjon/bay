//! G34. A moved base never hides an earlier landing.
//!
//! Executor E1 stalls with revision R1; R2 supersedes it; E1 wakes and lands R1 (a race only a
//! hook stops); the agent resumes, so its base moves past R1's landing; E2's landing of R2 is
//! refused `CHANGE_ALREADY_LANDED` because the scan starts at `base_first`; `status` shows the
//! change landed, never open.

mod support;

use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn the_scan_from_base_first_finds_the_earlier_landing() {
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

    // E1 stalls just before its publishing push, past its last ownership check.
    let e1 = test_box.clone_of(&remote, "e1");
    test_box.git_ok(&e1, &["config", "bay.producerName", "e1-checks"]);
    let release = test_box.path().join("release-e1");
    let child = test_box
        .bay_process(&e1)
        .env("BAY_TEST_PAUSE_AT", "before-publish")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["--json", "executor", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = test_box.path().join("release-e1.paused");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !paused.exists() {
        assert!(Instant::now() < deadline, "E1 never stalled");
        std::thread::sleep(Duration::from_millis(50));
    }

    // R2 supersedes R1; then E1 wakes and wins the race with R1.
    std::fs::write(view.join("src/r2.rs"), "fn r2() {}\n").unwrap();
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);
    std::fs::write(&release, "go").unwrap();
    assert!(child.wait_with_output().unwrap().status.success());
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/new.rs"]),
        "fn new() {}"
    );

    // The agent resumes: its base moves past R1's landing.
    let resumed = test_box.bay_ok(&repo, &["start", "--resume", &id]);
    assert_eq!(resumed["detail"]["trunk_moved"], true, "{resumed}");
    assert!(
        resumed["detail"]["landed"]["commit"].is_string(),
        "resume reports the landing: {resumed}"
    );

    let e2 = test_box.clone_of(&remote, "e2");
    test_box.git_ok(&e2, &["config", "bay.producerName", "e2-checks"]);
    let ran = test_box.bay_ok(&e2, &["executor", "run", "--once"]);
    let r2 = ran["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .find(|result| result["op_id"] == format!("{id}.2"))
        .unwrap_or_else(|| panic!("R2's operation did not run: {ran}"));
    assert_eq!(r2["reason"], "CHANGE_ALREADY_LANDED", "{ran}");
    assert_eq!(
        test_box.trunk_change_ids(&remote),
        vec![id.clone()],
        "R2 never lands"
    );
    let status = test_box.bay_ok(&repo, &["status", &id]);
    assert_eq!(
        status["detail"]["changes"][0]["state"], "landed",
        "{status}"
    );
}

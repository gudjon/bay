//! G757 (owner, 2026-11-16: a panel shows the head's check against the line's typical check
//! time, and an operation's `elapsed_s` counts from its request, not from when a runner took
//! it). An operation a runner holds names `claimed_s`, the seconds since that runner claimed it,
//! read from the claim's commit; one no runner holds names none.
//!
//! Guard to mutate: in `src/status.rs` `under_way`, drop `claimed_s`.

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

/// A `bay` process killed when the case ends.
struct Held(std::process::Child);

impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_held_change_says_how_long_since_its_claim() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    let before = test_box.bay_ok(&repo, &["status", &id]);
    let operation = &before["detail"]["changes"][0]["operation"];
    assert!(
        operation["claimed_s"].is_null(),
        "no runner holds it yet: {before}"
    );

    // WHEN a runner claims it and is held there
    let release = test_box.path().join("release-runner");
    let _runner = Held(
        test_box
            .bay_process(&repo)
            .env("BAY_TEST_PAUSE_AT", "after-persist")
            .env("BAY_TEST_PAUSE_FILE", &release)
            .args(["--json", "runner", "run", "--once"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let paused = PathBuf::from(format!("{}.paused", release.display()));
    let deadline = Instant::now() + Duration::from_secs(120);
    while !paused.exists() {
        assert!(Instant::now() < deadline, "the runner never claimed");
        std::thread::sleep(Duration::from_millis(50));
    }

    // THEN the change says how long since its claim, not since its request
    let held = test_box.bay_ok(&repo, &["status", &id]);
    let operation = &held["detail"]["changes"][0]["operation"];
    let claimed = operation["claimed_s"]
        .as_u64()
        .unwrap_or_else(|| panic!("no claimed_s: {held}"));
    assert!(claimed < 120, "{held}");
}

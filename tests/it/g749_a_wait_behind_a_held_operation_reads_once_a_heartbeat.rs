//! G749 (a fleet finding, 2026-11-16: on a box at load 100, each `bay merge --wait` behind an
//! operation a runner held fetched the change's chain five times a second, 3,000 to 7,700 Git
//! commands a wait, while the holder moves the chain at most once a heartbeat). A wait behind a
//! held operation reads the chain again once a heartbeat, never faster.
//!
//! Guard to mutate: in `src/publish.rs` `lease_expired`, sleep 200 ms between reads again, and
//! the wait fetches the chain about forty times in eight seconds.

// The holder runs as a shell-scripted check, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

/// A `bay` process killed when the case ends, so none outlives its test.
struct Held(std::process::Child);

impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_wait_behind_a_held_operation_reads_the_chain_once_a_heartbeat() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nlease_s = 60\nheartbeat_s = 3\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");

    // A runner claims the operation and is held there, holding its lease.
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

    // WHEN the author waits behind it for eight seconds, its fetches of the chain counted
    let fetches = test_box.path().join("chain-fetches");
    test_box.wrap_git(&format!(
        "case \" $* \" in\n  *\" fetch \"*refs/bay/changes/{id}*) echo x >> '{}' ;;\nesac",
        fetches.display()
    ));
    let _wait = Held(
        test_box
            .bay_process(&view)
            .args(["--json", "merge", "--wait"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_secs(8));

    // THEN it read the chain about once a heartbeat (3 s), not five times a second
    let count = std::fs::read_to_string(&fetches).map_or(0, |text| text.lines().count());
    assert!(
        count <= 12,
        "the wait fetched the chain {count} times in 8 s with a 3 s heartbeat"
    );
}

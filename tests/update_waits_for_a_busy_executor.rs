//! An update never replaces the binary under an executor that holds an operation, and an
//! executor restarts on a new binary between operations (owner ruling 2026-10-18).
//!
//! `bay executor run` holds a box-local lock while it owns an operation: a file named by its pid
//! in `$XDG_STATE_HOME/bay/executor.lock.d/`, one per executor, so two executors on one box
//! never erase each other's. `bay update` refuses `UPDATE_IN_USE` while any file names a live
//! pid (one it may not signal counts as live), and a stale file is ignored. Between operations the executor exits when its binary changed, so its
//! supervisor restarts it on the new one.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod release_fixture;
mod support;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use release_fixture::Fixture;
use release_fixture::RetryBusy as _;
use support::{TestBox, check_files, passing_result, result_json};

const NEWER: &str = "9.0.0";

/// A child killed when the test ends, so a failed assertion never leaves it running.
struct Reaped(std::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn eventually(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(60), "never: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn an_update_waits_while_a_live_executor_holds_the_lock() {
    let fixture = Fixture::new();
    fixture.release(NEWER, NEWER, None);
    fixture.list(&[(NEWER, false)]);
    let locks = fixture.path().join("state/bay/executor.lock.d");
    std::fs::create_dir_all(&locks).unwrap();
    let mut holder = Reaped(Command::new("sleep").arg("60").spawn_busy());
    std::fs::write(
        locks.join(holder.0.id().to_string()),
        format!("{}\n", holder.0.id()),
    )
    .unwrap();

    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "UPDATE_IN_USE");
    assert_eq!(result["allowed"], serde_json::json!(["wait"]));
    assert_eq!(result["retry_when"], "operation_finished");
    fixture.assert_untouched();

    // The background check does not start, and does not use up the day, while it is held.
    let repo = fixture.repo("");
    let due = fixture.checked_ago(2 * 24 * 3600);
    let _ = fixture.bay(&repo).arg("status").output_busy();
    assert_eq!(
        fixture.state().unwrap()["checked_at"].as_u64(),
        Some(due),
        "no check while an executor holds an operation"
    );

    // A lock whose pid is gone is stale and ignored.
    holder.0.kill().unwrap();
    holder.0.wait().unwrap();
    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["updated"], true);
}

#[test]
fn the_executor_holds_the_lock_while_it_owns_an_operation() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);
    let executor = test_box.clone_of(&remote, "executor");
    let locks = test_box.home.join(".local/state/bay/executor.lock.d");
    let release = test_box.path().join("release");
    let paused = test_box.path().join("release.paused");

    let mut child = Reaped(
        test_box
            .bay_process(&executor)
            .env("BAY_TEST_PAUSE_AT", "after-persist")
            .env("BAY_TEST_PAUSE_FILE", &release)
            .args(["--json", "executor", "run", "--once"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn_busy(),
    );
    eventually("the executor reaches its pause", || paused.exists());
    let lock = locks.join(child.0.id().to_string());
    assert_eq!(
        std::fs::read_to_string(&lock).unwrap().trim(),
        child.0.id().to_string(),
        "the lock names the executor while it owns the operation"
    );
    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .args(["update", "--version", NEWER])
            .env("BAY_DISABLE_AUTOUPDATE", "1")
            // Never the network: a release base that does not exist.
            .env(
                "BAY_TEST_RELEASE_URL",
                format!("file://{}", test_box.path().join("no-release").display()),
            ),
    );
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "UPDATE_IN_USE");

    std::fs::write(&release, "").unwrap();
    assert!(child.0.wait().unwrap().success());
    assert!(!lock.exists(), "the lock is released after the operation");
}

#[test]
fn an_executor_exits_when_its_binary_changes() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("a.txt", "a\n")]);
    let bin = test_box.path().join("exec-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let bay = bin.join("bay");
    std::fs::copy(env!("CARGO_BIN_EXE_bay"), &bay).unwrap();
    let path = format!(
        "{}:{}:/bin:/usr/bin",
        test_box.fakebin.display(),
        test_box.local_bin().display()
    );
    let mut child = Reaped(
        Command::new(&bay)
            .arg("-C")
            .arg(&repo)
            .args(["--json", "executor", "run", "--interval-s", "1"])
            .env_clear()
            .env("PATH", path)
            .env("HOME", &test_box.home)
            .env("GIT_CONFIG_GLOBAL", &test_box.global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", test_box.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn_busy(),
    );
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the executor keeps running while its binary is unchanged"
    );

    // An update's swap: a new file renamed over the path.
    let next = bin.join(".bay.next");
    std::fs::copy(env!("CARGO_BIN_EXE_bay"), &next).unwrap();
    std::fs::rename(&next, &bay).unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the executor never noticed its new binary"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        status.success(),
        "a clean exit, for the supervisor to restart"
    );
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.0.stdout.take().unwrap(), &mut out).unwrap();
    let result: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(result["decision"], "ok", "{result}");
    assert_eq!(
        result["detail"]["restart"], "the binary changed",
        "{result}"
    );
}

/// `bay update` in `dir`, never reaching the network.
fn update_offline(test_box: &TestBox, dir: &std::path::Path) -> (i32, serde_json::Value) {
    result_json(
        test_box
            .bay(dir)
            .args(["update", "--version", NEWER])
            .env("BAY_DISABLE_AUTOUPDATE", "1")
            .env(
                "BAY_TEST_RELEASE_URL",
                format!("file://{}", test_box.path().join("no-release").display()),
            ),
    )
}

/// A landed change in its own repository, and a paused executor driving it.
fn paused_executor(test_box: &TestBox, name: &str) -> (Reaped, std::path::PathBuf) {
    let repo = test_box.repo_any(name);
    let remote = test_box.path().join(format!("{name}-remote.git"));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);
    let executor = test_box.clone_of(&remote, &format!("{name}-executor"));
    let release = test_box.path().join(format!("{name}-release"));
    let paused = test_box.path().join(format!("{name}-release.paused"));
    let child = Reaped(
        test_box
            .bay_process(&executor)
            .env("BAY_TEST_PAUSE_AT", "after-persist")
            .env("BAY_TEST_PAUSE_FILE", &release)
            .args(["--json", "executor", "run", "--once"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn_busy(),
    );
    eventually("the executor reaches its pause", || paused.exists());
    (child, release)
}

#[test]
fn one_executor_releasing_never_frees_another_executors_hold() {
    let test_box = TestBox::new();
    let (mut a, _release_a) = paused_executor(&test_box, "a");
    let (mut b, release_b) = paused_executor(&test_box, "b");

    // B finishes its operation and releases its own hold; A still owns one.
    std::fs::write(&release_b, "").unwrap();
    assert!(b.0.wait().unwrap().success());
    let (code, result) = update_offline(&test_box, test_box.path());
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "UPDATE_IN_USE");
    assert!(
        result["detail"]["why"]
            .as_str()
            .unwrap()
            .contains(&a.0.id().to_string()),
        "{result}"
    );
    a.0.kill().unwrap();
}

#[test]
fn a_pid_that_cannot_be_signalled_counts_as_alive() {
    // Pid 1 exists on every box, and an unprivileged process may not signal it.
    let test_box = TestBox::new();
    let locks = test_box.home.join(".local/state/bay/executor.lock.d");
    std::fs::create_dir_all(&locks).unwrap();
    std::fs::write(locks.join("1"), "1\n").unwrap();
    let (code, result) = update_offline(&test_box, test_box.path());
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "UPDATE_IN_USE");
}

//! An update never waits for an executor, and an executor moves to a new binary between
//! operations (owner ruling 2026-10-20; G251 is the whole story).
//!
//! `bay runner run` names itself for as long as it runs: a file named by its pid in
//! `$XDG_STATE_HOME/bay/executor.lock.d/`, holding its pid and its version, one per executor, so
//! two executors on one box never erase each other's. `bay update` installs at once, keeps every
//! version a live pid's file names (one it may not signal counts as live), and prunes the rest
//! past the newest three; a stale file keeps nothing. Between passes a looping executor whose
//! binary changed names the move in its result and execs the new binary in its place.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::release_fixture;
use crate::support;

use std::io::BufRead as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bay::version::BAY;
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

/// The kept versions under `versions`, sorted by name.
fn kept(versions: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(versions)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn an_update_keeps_every_version_a_live_executor_names() {
    let fixture = Fixture::new();
    fixture.release(NEWER, NEWER, None);
    fixture.list(&[(NEWER, false)]);
    let locks = fixture.path().join("state/bay/executor.lock.d");
    std::fs::create_dir_all(&locks).unwrap();
    // Pid 1 exists on every box, and an unprivileged process may not signal it: it is live.
    std::fs::write(locks.join("1"), "1\n0.0.1\n").unwrap();
    // A pid that is gone is stale: the version it names may go.
    let mut gone = Command::new("true").spawn_busy();
    gone.wait().unwrap();
    std::fs::write(
        locks.join(gone.id().to_string()),
        format!("{}\n0.0.2\n", gone.id()),
    )
    .unwrap();
    let versions = fixture.home().join(".local/share/bay/versions");
    for version in ["0.0.1", "0.0.2", "0.0.3", "0.0.4", "0.0.5"] {
        std::fs::create_dir_all(versions.join(version)).unwrap();
        std::fs::write(versions.join(version).join("bay"), version).unwrap();
    }

    let (code, result) = fixture.update(fixture.path(), &[]);
    assert_eq!(code, 0, "an update never waits: {result}");
    assert_eq!(result["detail"]["updated"], true, "{result}");
    assert!(
        std::fs::read(versions.join(NEWER).join("bay")).unwrap()
            == std::fs::read(fixture.bin()).unwrap(),
        "the install path holds the release's own kept file"
    );
    assert!(
        std::fs::read(versions.join(BAY).join("bay")).unwrap()
            == std::fs::read(env!("CARGO_BIN_EXE_bay")).unwrap(),
        "the version the update replaced keeps its file"
    );
    let mut want = vec![
        NEWER.to_owned(),
        BAY.to_owned(),
        "0.0.5".to_owned(),
        "0.0.1".to_owned(),
    ];
    want.sort();
    assert_eq!(
        kept(&versions),
        want,
        "the newest three, and the live executor's"
    );
    release_fixture::assert_no_leftovers(&fixture);
}

#[test]
fn the_daily_check_runs_while_an_executor_is_live() {
    let fixture = Fixture::new();
    fixture.release(NEWER, NEWER, None);
    fixture.list(&[(NEWER, false)]);
    let locks = fixture.path().join("state/bay/executor.lock.d");
    std::fs::create_dir_all(&locks).unwrap();
    std::fs::write(locks.join("1"), "1\n0.0.1\n").unwrap();
    let repo = fixture.repo("");
    let due = fixture.checked_ago(2 * 24 * 3600);
    let _ = fixture.bay(&repo).arg("status").output_busy();
    assert!(
        fixture.state().unwrap()["checked_at"].as_u64() > Some(due),
        "a live executor does not hold off the check"
    );
    // The background update installs; the fixture outlives it.
    let installed = fixture.home().join(format!("installed-{NEWER}"));
    eventually("the background update installs", || installed.exists());
}

#[test]
fn the_executor_names_its_pid_and_version_while_it_runs() {
    let test_box = TestBox::new();
    let (mut child, release) = paused_executor(&test_box, "repo");
    let lock = test_box
        .home
        .join(".local/state/bay/executor.lock.d")
        .join(child.0.id().to_string());
    assert_eq!(
        std::fs::read_to_string(&lock).unwrap(),
        format!("{}\n{BAY}\n", child.0.id()),
        "the file names the executor and its version while it owns the operation"
    );
    // An update is never refused for it: this one fails only for want of a release.
    let (code, result) = update_offline(&test_box, test_box.path());
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "UPDATE_UNAVAILABLE", "{result}");

    std::fs::write(&release, "").unwrap();
    assert!(child.0.wait().unwrap().success());
    assert!(!lock.exists(), "the file is removed when the executor ends");
}

#[test]
fn an_executor_moves_to_its_new_binary_between_passes() {
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
            .args(["--json", "runner", "run", "--interval-s", "1"])
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
    let line = first_line(child.0.stdout.take().unwrap());
    let result: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(result["decision"], "ok", "{result}");
    let reexec = &result["detail"]["reexec"];
    assert_eq!(reexec["from"], BAY, "{result}");
    assert_eq!(reexec["to"], BAY, "{result}");
    assert_eq!(
        std::path::Path::new(reexec["path"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        bay.canonicalize().unwrap(),
        "{result}"
    );
    // The same process now runs the new binary, and names itself again.
    let lock = test_box
        .home
        .join(".local/state/bay/executor.lock.d")
        .join(child.0.id().to_string());
    eventually("the new binary runs as the executor", || lock.exists());
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "it moved in place, it did not exit"
    );
}

/// The first line `out` writes, within a minute.
fn first_line(out: impl std::io::Read + Send + 'static) -> String {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::BufReader::new(out).read_line(&mut line);
        let _ = send.send(line);
    });
    receive
        .recv_timeout(Duration::from_secs(60))
        .expect("the executor never noticed its new binary")
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
    let (code, _) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0);
    let executor = test_box.clone_of(&remote, &format!("{name}-executor"));
    let release = test_box.path().join(format!("{name}-release"));
    let paused = test_box.path().join(format!("{name}-release.paused"));
    let child = Reaped(
        test_box
            .bay_process(&executor)
            .env("BAY_TEST_PAUSE_AT", "after-persist")
            .env("BAY_TEST_PAUSE_FILE", &release)
            .args(["--json", "runner", "run", "--once"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn_busy(),
    );
    eventually("the executor reaches its pause", || paused.exists());
    (child, release)
}

#[test]
fn one_executor_ending_never_frees_another_executors_file() {
    let test_box = TestBox::new();
    let (mut a, _release_a) = paused_executor(&test_box, "a");
    let (mut b, release_b) = paused_executor(&test_box, "b");
    let locks = test_box.home.join(".local/state/bay/executor.lock.d");

    // B finishes its operation and removes its own file; A's stays.
    std::fs::write(&release_b, "").unwrap();
    assert!(b.0.wait().unwrap().success());
    assert!(!locks.join(b.0.id().to_string()).exists());
    assert_eq!(
        std::fs::read_to_string(locks.join(a.0.id().to_string())).unwrap(),
        format!("{}\n{BAY}\n", a.0.id())
    );
    a.0.kill().unwrap();
}

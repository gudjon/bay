//! G251: an update never waits for a running executor, and the executor moves to the new
//! version between operations (owner ruling 2026-10-20).
//!
//! While `bay executor run` holds an operation, `bay update` installs at once: the release is
//! kept as its version's own file under `$XDG_DATA_HOME/bay/versions/`, and one rename points
//! the install path at it. The executor pinned its own version's file there when it started and
//! starts every child `bay` from it; the update neither replaces nor prunes that file, so the
//! held operation ends on the version it began with. Once the operation is done, between
//! passes, the executor writes its result naming the move (`detail.reexec`) and execs the
//! install path, now the new version, with the same arguments, under the same pid.
//!
//! Mutations that fail it: the `UPDATE_IN_USE` refusal back in `update::run` (the update is
//! refused); `place` writing over the running version's file, or `prune` ignoring
//! `executor::live_versions` (the kept file changes or goes); `run` not returning `reexec` once
//! the binary changed (no move is named); `executor::reexec` not exec-ing (the new version
//! never runs).

// The executor's new version is a shell script, and the move is an `exec`: Unix only.
#![cfg(unix)]

use std::io::BufRead as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bay::version::BAY;

use crate::release_fixture::{self, Fixture, RetryBusy as _};
use crate::support::{TestBox, check_files, passing_result, result_json};

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
fn an_update_never_waits_and_the_executor_moves_between_operations() {
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

    // bay as installed: the executor runs the install path, and loops.
    std::fs::create_dir_all(test_box.local_bin()).unwrap();
    let installed = test_box.local_bin().join("bay");
    std::fs::copy(env!("CARGO_BIN_EXE_bay"), &installed).unwrap();
    let release = test_box.path().join("release");
    let paused = test_box.path().join("release.paused");
    let mut child = held_executor(&test_box, &installed, &executor, &release);
    eventually("the executor holds its operation", || paused.exists());
    let pid = child.0.id();
    let presence = test_box
        .home
        .join(".local/state/bay/executor.lock.d")
        .join(pid.to_string());
    assert_eq!(
        std::fs::read_to_string(&presence).unwrap(),
        format!("{pid}\n{BAY}\n"),
        "the executor names its version"
    );
    let versions = test_box.home.join(".local/share/bay/versions");
    let own = versions.join(BAY).join("bay");
    let build = std::fs::read(env!("CARGO_BIN_EXE_bay")).unwrap();
    assert!(
        std::fs::read(&own).unwrap() == build,
        "the executor's children start its own version's kept file"
    );

    // The update, while the operation is held: it installs at once.
    let started = Instant::now();
    let (code, result) = update_now(&test_box, &repo);
    assert_eq!(code, 0, "an update never waits for the executor: {result}");
    assert_eq!(result["detail"]["updated"], true, "{result}");
    assert!(started.elapsed() < Duration::from_secs(60), "at once");
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the operation is still held"
    );
    let printed = release_fixture::run(Command::new(&installed).arg("--version"));
    assert_eq!(printed.trim(), format!("bay {NEWER}"));
    assert!(
        std::fs::read(versions.join(NEWER).join("bay")).unwrap()
            == std::fs::read(&installed).unwrap(),
        "the install path holds the release's own kept file"
    );
    assert!(
        std::fs::read(&own).unwrap() == build,
        "the held operation's children still start the version it began with"
    );

    // The operation ends on its version; between passes the executor names the move.
    std::fs::write(&release, "").unwrap();
    let line = first_line(child.0.stdout.take().unwrap());
    let result: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(result["decision"], "ok", "{result}");
    let ran = result["detail"]["ran"].as_array().unwrap();
    assert!(
        ran.iter().any(|op| op["decision"] == "landed"),
        "the held operation landed before the move: {result}"
    );
    let reexec = &result["detail"]["reexec"];
    assert_eq!(reexec["from"], BAY, "{result}");
    assert_eq!(reexec["to"], NEWER, "{result}");
    assert_eq!(
        Path::new(reexec["path"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        installed.canonicalize().unwrap(),
        "{result}"
    );

    // Then the new version runs in its place, with the same arguments; it logs and exits.
    let ran_new = test_box.home.join(format!("ran-{NEWER}"));
    eventually("the new version runs as the executor", || ran_new.exists());
    let args = std::fs::read_to_string(&ran_new).unwrap();
    assert_eq!(
        args.trim(),
        format!(
            "-C {} --json executor run --interval-s 1",
            executor.display()
        )
    );
    let status = child.0.wait().unwrap();
    assert_eq!(status.code(), Some(1), "the new version's own exit");
}

/// The first line `out` writes, within two minutes.
fn first_line(out: impl std::io::Read + Send + 'static) -> String {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::BufReader::new(out).read_line(&mut line);
        let _ = send.send(line);
    });
    receive
        .recv_timeout(Duration::from_secs(120))
        .expect("the executor never named its move")
}

/// The installed bay running an executor loop on `executor`, held after it persists its
/// operation until `release` exists.
fn held_executor(test_box: &TestBox, installed: &Path, executor: &Path, release: &Path) -> Reaped {
    let path = format!(
        "{}:{}:/bin:/usr/bin",
        test_box.fakebin.display(),
        test_box.local_bin().display()
    );
    Reaped(
        Command::new(installed)
            .arg("-C")
            .arg(executor)
            .args(["--json", "executor", "run", "--interval-s", "1"])
            .env_clear()
            .env("PATH", path)
            .env("HOME", &test_box.home)
            .env("GIT_CONFIG_GLOBAL", &test_box.global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", test_box.path())
            .env(
                "BAY_TEST_USERNS_RESTRICT",
                test_box.path().join("userns-restrict"),
            )
            .env("BAY_TEST_PAUSE_AT", "after-persist")
            .env("BAY_TEST_PAUSE_FILE", release)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn_busy(),
    )
}

/// `bay update --version NEWER` against the fake signed release.
fn update_now(test_box: &TestBox, repo: &Path) -> (i32, serde_json::Value) {
    let fixture = Fixture::new();
    fixture.release(NEWER, NEWER, None);
    let key = Fixture::public_line(&fixture.path().join("keys/release"));
    result_json(
        test_box
            .bay(repo)
            .args(["update", "--version", NEWER])
            .env("BAY_DISABLE_AUTOUPDATE", "1")
            .env(
                "BAY_TEST_RELEASE_URL",
                format!("file://{}", fixture.path().join("release").display()),
            )
            .env("BAY_TEST_RELEASE_KEY", key),
    )
}

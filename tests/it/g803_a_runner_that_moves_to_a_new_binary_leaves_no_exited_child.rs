//! G803 (a fleet finding, 2026-11-20: a runner held 36 exited `git` children, one for each new
//! binary it had moved to in six days). A runner that moves to a new binary leaves no exited
//! child behind.
//!
//! A runner keeps one `git cat-file --batch` of its store open for as long as it runs (G719).
//! When it moves to a new binary (G251) it execs it under the same pid: the process keeps its
//! children, and the new binary waits for none it did not start. So the runner ends its open
//! readers, and waits for them, before the exec.
//!
//! Guard to mutate, in `src/executor.rs`: `reexec`'s call of `git::close_open_readers` (drop
//! it, and the reader exits after the exec as a child nobody waits for).

// The new binary is a shell script, and the move is an `exec`: Unix only.
#![cfg(unix)]

use std::io::BufRead as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::release_fixture::{Build, Fixture, RetryBusy as _};
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

#[test]
fn a_runner_that_moves_to_a_new_binary_leaves_no_exited_child() {
    // GIVEN a looping runner, run from the install path, that landed a change: it read its
    // store's objects, so it holds an open reader
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0);
    let executor = test_box.clone_of(&remote, "executor");
    std::fs::create_dir_all(test_box.local_bin()).unwrap();
    let installed = test_box.local_bin().join("bay");
    std::fs::copy(env!("CARGO_BIN_EXE_bay"), &installed).unwrap();
    let mut runner = looping(&test_box, &installed, &executor);
    let began = Instant::now();
    while !test_box.trunk_change_ids(&remote).contains(&id) {
        assert!(
            began.elapsed() < Duration::from_secs(120),
            "the runner never landed the change"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // WHEN a new binary is installed: between passes the runner moves to it
    let fixture = Fixture::new();
    fixture.release_with(
        NEWER,
        NEWER,
        None,
        &Build {
            lingers: true,
            ..Build::default()
        },
    );
    let key = Fixture::public_line(&fixture.path().join("keys/release"));
    let (code, updated) = result_json(
        test_box
            .bay(&repo)
            .args(["update", "--version", NEWER])
            .env("BAY_DISABLE_AUTOUPDATE", "1")
            .env(
                "BAY_TEST_RELEASE_URL",
                format!("file://{}", fixture.path().join("release").display()),
            )
            .env("BAY_TEST_RELEASE_KEY", key),
    );
    assert_eq!(code, 0, "{updated}");
    let line = first_line(runner.0.stdout.take().unwrap());
    let moved: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(moved["detail"]["reexec"]["to"], NEWER, "{moved}");
    let ran = test_box.home.join(format!("ran-{NEWER}"));
    let began = Instant::now();
    while !std::fs::read_to_string(&ran).is_ok_and(|args| args.ends_with('\n')) {
        assert!(
            began.elapsed() < Duration::from_secs(60),
            "the new binary never ran"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // THEN the process, now the new binary, holds no exited child
    let pid = runner.0.id();
    let began = Instant::now();
    while began.elapsed() < Duration::from_secs(3) {
        let exited = exited_children(pid);
        assert!(
            exited.is_empty(),
            "the runner left exited children {exited:?} when it moved"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The installed bay running a runner loop on `executor`.
fn looping(test_box: &TestBox, installed: &Path, executor: &Path) -> Reaped {
    let path = format!(
        "{}:{}:/bin:/usr/bin",
        test_box.fakebin.display(),
        test_box.local_bin().display()
    );
    Reaped(
        Command::new(installed)
            .arg("-C")
            .arg(executor)
            .args(["--json", "runner", "run", "--interval-s", "1"])
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
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn_busy(),
    )
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
        .expect("the runner never named its move")
}

/// The children of `pid` that exited and nobody waited for, as `ps` lists them.
fn exited_children(pid: u32) -> Vec<String> {
    let out = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,stat=,comm="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            fields.get(1) == Some(&pid.to_string().as_str())
                && fields.get(2).is_some_and(|stat| stat.starts_with('Z'))
        })
        .map(str::to_owned)
        .collect()
}

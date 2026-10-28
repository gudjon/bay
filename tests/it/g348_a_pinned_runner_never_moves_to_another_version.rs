//! G348 (owner ruling 2026-10-24, BACKLOG 67 item 12: "a pin means one version"): a pinned
//! runner never moves to another version.
//!
//! A looping `bay runner run` whose binary changed moves to the new one between passes (G251).
//! On a box where several people share one `~/.local/bin/bay`, anyone's `bay update` or
//! install changes it, and the runner followed: an unreleased build reached a fleet's runner
//! that way, though its supervisor pinned a version (`BAY_UPDATE_VERSION`). A pin names one
//! version, so a runner with a pin moves only to the binary that answers that version; to any
//! other it says once why it stays, and keeps running its own. A pin it cannot read holds it too.
//!
//! Guard to mutate: `src/executor.rs` `pinned_elsewhere`: return `None` at once (the pinned
//! runner execs the other version, and exits).

// The new binary is a shell script, and the move is an `exec`: Unix only.
#![cfg(unix)]

use std::io::BufRead as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::release_fixture::RetryBusy as _;
use crate::support::{TestBox, check_files, passing_result};

/// A child killed when the test ends, so a failed assertion never leaves it running.
struct Reaped(std::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A bay of another version that takes every command (`--help` included) and exits at once:
/// a runner that moved to it would end.
const OTHER_VERSION: &str = "#!/bin/sh\nfor a in \"$@\"; do\n  [ \"$a\" = --version ] && { echo 'bay 9.0.0'; exit 0; }\ndone\nexit 0\n";

#[test]
fn a_pinned_runner_stays_on_its_version_when_its_binary_becomes_another() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let runner = test_box.clone_of(&remote, "runner");
    std::fs::create_dir_all(test_box.local_bin()).unwrap();
    let installed = test_box.local_bin().join("bay");
    std::fs::copy(env!("CARGO_BIN_EXE_bay"), &installed).unwrap();
    let version = String::from_utf8(
        Command::new(&installed)
            .arg("--version")
            .output_busy()
            .stdout,
    )
    .unwrap()
    .split_whitespace()
    .nth(1)
    .unwrap()
    .to_owned();
    let mut child = looping(&test_box, &installed, &runner, &version);
    let (lines, receive) = std::sync::mpsc::channel();
    let stderr = child.0.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
        {
            let _ = lines.send(line);
        }
    });
    // Let the first pass start on the build, then swap another version in under it.
    std::thread::sleep(Duration::from_secs(2));
    let swapped = test_box.path().join("bay-other-version");
    std::fs::write(&swapped, OTHER_VERSION).unwrap();
    std::fs::set_permissions(
        &swapped,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    std::fs::rename(&swapped, &installed).unwrap();

    let start = Instant::now();
    let stays = loop {
        let left = Duration::from_secs(60).saturating_sub(start.elapsed());
        match receive.recv_timeout(left) {
            Ok(line) if line.contains("keeps running bay") => break line,
            Ok(_) => {}
            Err(error) => panic!(
                "the runner never said it stays ({error}); exited: {:?}",
                child.0.try_wait()
            ),
        }
    };
    assert!(
        stays.contains(&format!(
            "is bay 9.0.0, but this box is pinned to bay {version}"
        )),
        "{stays}"
    );
    // Two more passes: still the same process, still running.
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the pinned runner moved to another version and exited"
    );
}

/// The installed bay running a runner loop on `runner`, pinned to `pin` as a supervisor pins it.
fn looping(test_box: &TestBox, installed: &Path, runner: &Path, pin: &str) -> Reaped {
    let path = format!(
        "{}:{}:/bin:/usr/bin",
        test_box.fakebin.display(),
        test_box.local_bin().display()
    );
    Reaped(
        Command::new(installed)
            .arg("-C")
            .arg(runner)
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
            .env("BAY_UPDATE_VERSION", pin)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn_busy(),
    )
}

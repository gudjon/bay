//! G337: a runner never moves to a bay that lacks its command.
//!
//! A looping `bay runner run` whose binary changed moves to the new one between passes (G251).
//! A new binary that answers as bay but does not take the runner's own arguments, such as a
//! bay from before the command was named `runner`, would only exit, and the runner's supervisor
//! would start it again and again: nothing would merge. So the runner first asks the new binary
//! for that command's `--help`; when it refuses, the runner says once why it stays, and keeps
//! running its own version.
//!
//! Guard to mutate: `src/executor.rs` `run`: `moves && binary.takes_arguments()` to `moves`
//! (the runner execs the binary that refuses its command, and exits).

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

/// A bay that answers `--version` and refuses every command, as a bay without `runner` does.
const WITHOUT_RUNNER: &str = "#!/bin/sh\nfor a in \"$@\"; do\n  [ \"$a\" = --version ] && { echo 'bay 9.0.0'; exit 0; }\ndone\necho \"error: unrecognized subcommand 'runner'\" >&2\nexit 2\n";

#[test]
fn a_runner_stays_on_its_version_when_the_new_binary_lacks_its_command() {
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
    let mut child = looping(&test_box, &installed, &runner);
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
    // Let the first pass start on the build, then swap the binary under it.
    std::thread::sleep(Duration::from_secs(2));
    let swapped = test_box.path().join("bay-without-runner");
    std::fs::write(&swapped, WITHOUT_RUNNER).unwrap();
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
        stays.contains("does not take this runner's command"),
        "{stays}"
    );
    // Two more passes: still the same process, still running.
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the runner moved to a bay without its command and exited"
    );
}

/// The installed bay running a runner loop on `runner`.
fn looping(test_box: &TestBox, installed: &Path, runner: &Path) -> Reaped {
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
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn_busy(),
    )
}

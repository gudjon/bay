//! G892. A serving origin moves to a new bay between passes, as a runner does.
//!
//! `bay host serve` runs for days under a supervisor. Before G892 it kept running the deleted
//! file of the release it started with, whatever the box installed after it, until a person
//! restarted it (a fleet finding, 2026-11-24: an origin ran 0.7.7's deleted file while its box ran 0.7.10).
//! Now, between two passes, an origin whose binary file changed asks it the runner's questions
//! (it answers as bay, it takes this origin's arguments, the box's pin allows it) and execs it in
//! place: the same pid, the same arguments.
//!
//! Mutation that fails it: drop the call of `between_passes` in `origin::run_serve` (the new
//! version never runs, and the origin never says it moves).

// The new version is a shell script, and the move is an `exec`: Unix only.
#![cfg(unix)]

use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::release_fixture::RetryBusy as _;
use crate::support::{TestBox, check_files, formats, passing_result};

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

/// The value of `name` in the bare repository `bare`, `None` when it has none.
fn value(test_box: &TestBox, bare: &Path, name: &str) -> Option<String> {
    let out = test_box
        .git(test_box.path())
        .arg(format!("--git-dir={}", bare.display()))
        .args(["rev-parse", "--verify", "-q", name])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
}

#[test]
fn a_serving_origin_execs_the_new_bay_with_its_own_arguments() {
    let test_box = TestBox::new();
    let hosted = test_box.bare_any("hosted.git");
    let origin = test_box.path().join("origin.git");
    let (object_format, _) = formats();
    test_box.bay_ok(
        test_box.path(),
        &[
            "host",
            origin.to_str().unwrap(),
            "--object-format",
            &object_format,
            "--upstream",
            hosted.to_str().unwrap(),
        ],
    );
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &hosted);
    let trunk = test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );

    // bay as installed serves the origin, and loops.
    std::fs::create_dir_all(test_box.local_bin()).unwrap();
    let installed = test_box.local_bin().join("bay");
    std::fs::copy(env!("CARGO_BIN_EXE_bay"), &installed).unwrap();
    let path = format!(
        "{}:{}:/bin:/usr/bin",
        test_box.fakebin.display(),
        test_box.local_bin().display()
    );
    let mut child = Reaped(
        Command::new(&installed)
            .arg("-C")
            .arg(test_box.path())
            .args(["host", "serve", origin.to_str().unwrap()])
            .env_clear()
            .env("PATH", path)
            .env("HOME", &test_box.home)
            .env("GIT_CONFIG_GLOBAL", &test_box.global_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CEILING_DIRECTORIES", test_box.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn_busy(),
    );
    // Its first pass takes the hosted remote's main: it runs, and holds its binary's file.
    eventually("the origin's first pass takes main", || {
        value(&test_box, &origin, "refs/heads/main").as_deref() == Some(trunk.as_str())
    });

    // The box installs a newer bay by a rename over the install path, as an update does. It
    // answers as bay, takes any command with `--help`, logs its arguments and exits.
    let ran_new = test_box.home.join(format!("ran-{NEWER}"));
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in --version) echo \"bay {NEWER}\"; exit 0;; esac\nfor a; do [ \"$a\" = --help ] && exit 0; done\necho \"$*\" >> {}\nexit 1\n",
        ran_new.display()
    );
    let next = test_box.local_bin().join(".bay.next");
    std::fs::write(&next, script).unwrap();
    let mut mode = std::fs::metadata(&next).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
    std::fs::set_permissions(&next, mode).unwrap();
    std::fs::rename(&next, &installed).unwrap();

    // Between passes the same process becomes the new version, with the same arguments.
    eventually("the new version runs as the origin", || {
        std::fs::read_to_string(&ran_new).is_ok_and(|args| args.ends_with('\n'))
    });
    let args = std::fs::read_to_string(&ran_new).unwrap();
    assert_eq!(
        args.trim(),
        format!(
            "-C {} host serve {}",
            test_box.path().display(),
            origin.display()
        )
    );
    let status = child.0.wait().unwrap();
    assert_eq!(status.code(), Some(1), "the new version's own exit");
    let mut said = String::new();
    child
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut said)
        .unwrap();
    assert!(
        said.contains(&format!("bay origin: moves to bay {NEWER}")),
        "{said}"
    );
}

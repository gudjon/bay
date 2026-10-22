//! G128: a held ref lock on the remote is a retry, never a failure, and bay never unlinks it.
//!
//! Another process on the remote holding trunk's lock (`refs/heads/main.lock` on files,
//! `reftable/tables.list.lock` on reftable) makes the publishing push fail with "cannot lock
//! ref". Trunk did not move, so the same candidate is pushed again after a short wait,
//! uncounted toward `MAIN_MOVED`. A lock that is never released ends the operation
//! `MERGE_RETRY_EXHAUSTED`, a stated refusal, and the lock is still where its owner left it.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

/// Kills a background process if the test ends before it does.
struct Killed(Option<std::process::Child>);

impl Drop for Killed {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The file whose presence holds trunk's lock on `remote`, by its ref backend.
fn trunk_lock(test_box: &TestBox, remote: &Path) -> PathBuf {
    if test_box.git_bare(remote, &["rev-parse", "--show-ref-format"]) == "reftable" {
        remote.join("reftable/tables.list.lock")
    } else {
        remote.join("refs/heads/main.lock")
    }
}

/// A submitted change, and an executor held just before its publishing push, with `env` added
/// to its environment.
fn paused_before_publish(
    test_box: &TestBox,
    env: &[(&str, &str)],
) -> (String, PathBuf, PathBuf, Killed) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, submitted) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    let executor = test_box.clone_of(&remote, "executor");
    let release = test_box.path().join("release");
    let child = Killed(Some(
        test_box
            .bay_process(&executor)
            .env("BAY_TEST_PAUSE_AT", "before-publish")
            .env("BAY_TEST_PAUSE_FILE", &release)
            .envs(env.iter().copied())
            .args(["--json", "runner", "run", "--once"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    ));
    let deadline = Instant::now() + Duration::from_secs(60);
    while !test_box.path().join("release.paused").exists() {
        assert!(
            Instant::now() < deadline,
            "the executor never reached the push"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    (id, remote, release, child)
}

#[test]
fn a_lock_released_meanwhile_is_waited_out_and_the_change_lands() {
    let test_box = TestBox::new();
    let (id, remote, release, mut child) = paused_before_publish(&test_box, &[]);
    let lock = trunk_lock(&test_box, &remote);
    std::fs::write(&lock, "").unwrap();
    std::fs::write(&release, "go").unwrap();
    // Its owner finishes a moment later.
    std::thread::sleep(Duration::from_millis(1500));
    std::fs::remove_file(&lock).unwrap();
    let output = child.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

#[test]
fn a_lock_never_released_is_a_stated_refusal_and_stays_in_place() {
    never_released(&[]);
}

/// G104: Git's messages are read in the C locale, so a box whose locale translates them (here
/// `LANGUAGE=de`) still reads a held lock as one.
#[test]
fn a_held_lock_is_read_as_one_whatever_the_boxs_language() {
    never_released(&[("LANG", "en_US.UTF-8"), ("LANGUAGE", "de")]);
}

fn never_released(env: &[(&str, &str)]) {
    let test_box = TestBox::new();
    let (id, remote, release, mut child) = paused_before_publish(&test_box, env);
    let trunk = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let lock = trunk_lock(&test_box, &remote);
    std::fs::write(&lock, "held by another process\n").unwrap();
    std::fs::write(&release, "go").unwrap();
    let output = child.0.take().unwrap().wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("MERGE_RETRY_EXHAUSTED"), "{text}");
    assert!(
        !text.contains("\"decision\":\"failed\""),
        "never failed: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(&lock).unwrap(),
        "held by another process\n",
        "bay never unlinks another process's lock"
    );
    std::fs::remove_file(&lock).unwrap();
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
    assert!(
        test_box
            .trunk_change_ids(&remote)
            .iter()
            .all(|landed| *landed != id)
    );
}

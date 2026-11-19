//! G799 (a gate finding, 2026-11-19: a worker outage left a change's own run holding its
//! operation, and a looping runner answered it held, on a read older than the wait that run had
//! recorded, so it never took the wait over). A looping runner's answer that another runner
//! holds an operation is judged on the chain as the remote holds it at the visit, as the claim
//! judges it: once the holder recorded that no worker answered, the runner takes the operation
//! at once.
//!
//! Guard to mutate, in `src/publish.rs` `settled_by_what_the_pass_read`: drop the chain's read
//! before the lease (the runner answers the lease held from its pass's read).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

/// A runner's clone of `remote` named `name`.
fn runner(test_box: &TestBox, remote: &Path, name: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    clone
}

/// Waits until `path` exists.
fn until_exists(path: &Path, what: &str) {
    let began = Instant::now();
    while !path.exists() {
        assert!(began.elapsed() < Duration::from_secs(120), "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_looping_runner_takes_a_wait_its_pass_read_as_held() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    // The check's first run waits for the test, then finds its worker busy; later runs pass.
    let dir = test_box.path().to_path_buf();
    let count = test_box.relay(&format!(
        "printf x >> '{runs}'; n=$(wc -c < '{runs}' | tr -d ' '); if [ \"$n\" -eq 1 ]; then : > '{held}'; until [ -e '{go}' ] || ! kill -0 $PPID 2>/dev/null; do sleep 0.05; done; fi; echo \"$n\"",
        runs = dir.join("runs").display(),
        held = dir.join("held").display(),
        go = dir.join("go").display(),
    ));
    let before = format!(
        "n=$({count})\nif [ \"$n\" -eq 1 ]; then printf '%s\\n' '{{\"schema\":\"bay.check.result/1\",\"unavailable\":\"the worker did not start the check within 120 s\"}}'; exit 0; fi"
    );
    let files: Vec<(String, String)> = check_files(&before, &passing_result(1), 1, 60)
        .into_iter()
        .chain([(
            ".bay/config.toml".to_owned(),
            "[runner]\nlease_s = 60\nheartbeat_s = 1\n".to_owned(),
        )])
        .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    let op = queued["op_id"].as_str().unwrap().to_owned();
    let a = runner(&test_box, &remote, "exec-a");
    let b = runner(&test_box, &remote, "exec-b");

    // GIVEN runner A holds the change in its check
    let holder = test_box
        .bay_process(&a)
        .args(["--json", "runner", "run", "--once"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    until_exists(&dir.join("held"), "runner A's check never started");
    // AND a looping runner B read its chain so, and waits at the start of its visit
    let journal = dir.join("journal");
    let resume = dir.join("resume");
    let mut looping = test_box
        .bay_process(&b)
        .args(["--json", "runner", "run", "--interval-s", "1"])
        .env("BAY_TEST_PAUSE_AT", "visit-begins")
        .env("BAY_TEST_PAUSE_FILE", &resume)
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&journal).unwrap())
        .spawn()
        .unwrap();
    until_exists(
        &resume.with_extension("paused"),
        "runner B never began its visit",
    );

    // WHEN runner A's worker cannot start the check, and A records the wait
    std::fs::write(dir.join("go"), "").unwrap();
    let out = holder.wait_with_output().unwrap();
    assert!(out.status.success());
    std::fs::write(&resume, "").unwrap();

    // THEN runner B takes it at once, never answering it held, and it lands
    let chain = format!("refs/bay/changes/{id}");
    let began = Instant::now();
    while !test_box
        .git_bare(&remote, &["log", "--format=%s", &chain])
        .contains("by exec-b")
        && began.elapsed() < Duration::from_secs(60)
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    let landed = Instant::now();
    while !test_box.trunk_change_ids(&remote).contains(&id)
        && landed.elapsed() < Duration::from_secs(60)
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = looping.kill();
    let _ = looping.wait();
    let lines = std::fs::read_to_string(&journal).unwrap_or_default();
    let held: Vec<&str> = lines
        .lines()
        .filter(|line| line.contains(&format!("{op} [")) && line.contains("] ok ("))
        .collect();
    assert!(held.is_empty(), "runner B answered the wait held: {held:?}");
    assert!(
        test_box.trunk_change_ids(&remote).contains(&id),
        "the change never landed: {lines}"
    );
}

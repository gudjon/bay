//! G807 (a fleet finding, 2026-11-20: a second runner started under another live runner's
//! `bay.producerName`, and in 100 minutes claimed eleven heads, each released after a fault only
//! when its evidence push collided with the other's, two of them after a full 16-minute check). A
//! runner whose producer a live presence names under another runner name is refused at its start
//! with `PRODUCER_SHARED`, naming that runner: it publishes no presence and claims nothing, and
//! `bay doctor --role runner` names it in the row that reports the runner's producer. A runner
//! already running yields only to one whose name sorts before its own, so of two that started at
//! once, before either read the other, one goes on.
//!
//! Guard to mutate, in `src/executor.rs`: `producer_shared` answering `None` (the second runner
//! starts, publishes its presence and claims the head; a running runner claims under a producer
//! a runner named before it writes as).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, check_files, passing_result};

const SHARED: &str = "shared-producer";

/// A repository whose one check passes, and its remote.
fn setup(test_box: &TestBox) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = check_files("", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (repo, remote)
}

/// A runner's clone of `remote`, named `runner`, writing evidence as `producer`.
fn runner(test_box: &TestBox, remote: &Path, runner: &str, producer: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, runner);
    test_box.git_ok(&clone, &["config", "bay.runnerName", runner]);
    test_box.git_ok(&clone, &["config", "bay.producerName", producer]);
    clone
}

fn once(test_box: &TestBox, clone: &Path) -> Value {
    let out = test_box
        .bay(clone)
        .args(["--json", "runner", "run", "--once", "--interval-s", "300"])
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stdout)))
}

/// A change submitted to the line from a view of `repo`; returns its id.
fn queued(test_box: &TestBox, repo: &Path) -> String {
    let (id, view) = test_box.start_and_edit(repo);
    let out = test_box
        .bay(&view)
        .args(["--json", "merge"])
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["decision"], "queued", "{result}");
    id
}

/// The subjects of change `id`'s chain on `remote`.
fn chain(test_box: &TestBox, remote: &Path, id: &str) -> Vec<String> {
    test_box
        .git_bare(
            remote,
            &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Pushes to `remote` a live presence of runner `name`, on machine `elsewhere`, writing as
/// `producer`, as a runner that started at the same time as the one under test would have.
fn presence_of(test_box: &TestBox, remote: &Path, clone: &Path, name: &str, producer: &str) {
    let at = String::from_utf8(
        std::process::Command::new("date")
            .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let record = serde_json::json!({
        "schema": "bay.executor/1", "executor": name, "machine": "elsewhere",
        "producer": producer, "environments": {"source": "default", "declared": []},
        "at": at.trim(), "cadence_s": 300, "holding": null, "queue_depth": 0,
    });
    let blob = pipe(
        test_box,
        clone,
        &["hash-object", "-w", "--stdin"],
        &record.to_string(),
    );
    let tree = pipe(
        test_box,
        clone,
        &["mktree"],
        &format!("100644 blob {blob}\tpresence.json\n"),
    );
    let commit = test_box.git_ok(
        clone,
        &["commit-tree", &tree, "--no-gpg-sign", "-m", "presence"],
    );
    test_box.git_ok(
        clone,
        &[
            "push",
            "-q",
            &remote.display().to_string(),
            &format!("+{}:refs/bay/executor/{name}", commit.trim()),
        ],
    );
}

fn pipe(test_box: &TestBox, dir: &Path, args: &[&str], input: &str) -> String {
    use std::io::Write as _;
    let mut child = test_box
        .git(dir)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn a_runner_started_under_a_live_runners_producer_is_refused_and_doctor_names_it() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    // GIVEN runner A is live, writing as a producer
    let a = runner(&test_box, &remote, "exec-a", SHARED);
    assert_eq!(once(&test_box, &a)["decision"], "ok");
    let id = queued(&test_box, &repo);
    // WHEN runner B starts under the same producer
    let b = runner(&test_box, &remote, "exec-b", SHARED);
    let result = once(&test_box, &b);
    // THEN it is refused, naming runner A, before it publishes or claims anything
    assert_eq!(result["reason"], "PRODUCER_SHARED", "{result}");
    assert_eq!(
        result["detail"]["shared_with"]["runner"], "exec-a",
        "{result}"
    );
    assert!(
        !chain(&test_box, &remote, &id)
            .iter()
            .any(|subject| subject.contains("by exec-b")),
        "{:#?}",
        chain(&test_box, &remote, &id)
    );
    assert!(
        test_box
            .git_bare(&remote, &["for-each-ref", "refs/bay/executor/exec-b"])
            .is_empty(),
        "runner B published its presence"
    );
    // AND doctor names it in the row that reports the runner's producer
    let out = test_box
        .bay(&b)
        .args(["--json", "doctor", "--role", "runner"])
        .output()
        .unwrap();
    let doctor: Value = serde_json::from_slice(&out.stdout).unwrap();
    let row = doctor["detail"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == "runner-keys"))
        .cloned()
        .unwrap_or_else(|| panic!("{doctor}"));
    assert_eq!(row["state"], "red", "{row}");
    assert_eq!(row["shared_with"][0]["runner"], "exec-a", "{row}");
}

#[test]
fn a_running_runner_yields_only_to_a_runner_whose_name_sorts_first() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    // GIVEN runner B runs, and a runner named after it appears under its producer
    let b = runner(&test_box, &remote, "exec-b", SHARED);
    let journal = test_box.path().join("b-journal");
    let mut looping = test_box
        .bay_process(&b)
        .args(["--json", "runner", "run", "--interval-s", "1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&journal).unwrap())
        .spawn()
        .unwrap();
    let began = Instant::now();
    while test_box
        .git_bare(&remote, &["for-each-ref", "refs/bay/executor/exec-b"])
        .is_empty()
    {
        assert!(
            began.elapsed() < Duration::from_secs(60),
            "runner B never published"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    presence_of(&test_box, &remote, &b, "exec-c", SHARED);
    // THEN it goes on: a change lands
    let first = queued(&test_box, &repo);
    let began = Instant::now();
    while !chain(&test_box, &remote, &first)
        .iter()
        .any(|subject| subject.starts_with("landed"))
    {
        assert!(
            began.elapsed() < Duration::from_secs(120),
            "runner B stopped for a runner named after it: {}",
            std::fs::read_to_string(&journal).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    // WHEN a runner named before it appears under its producer
    presence_of(&test_box, &remote, &b, "exec-a", SHARED);
    let began = Instant::now();
    while !std::fs::read_to_string(&journal)
        .unwrap_or_default()
        .contains("same name as another")
    {
        assert!(
            began.elapsed() < Duration::from_secs(60),
            "runner B never yielded: {}",
            std::fs::read_to_string(&journal).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    // THEN it claims nothing more
    let second = queued(&test_box, &repo);
    std::thread::sleep(Duration::from_secs(8));
    let _ = looping.kill();
    let _ = looping.wait();
    assert!(
        !chain(&test_box, &remote, &second)
            .iter()
            .any(|subject| subject.starts_with("claim")),
        "{:#?}",
        chain(&test_box, &remote, &second)
    );
}

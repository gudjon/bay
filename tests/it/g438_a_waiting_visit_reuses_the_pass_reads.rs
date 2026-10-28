//! G438. A visit that only waits reuses what its pass read; one that runs reads afresh.
//!
//! A runner's pass visits every waiting change, and each visit fetched main, the registry and the
//! queue, and the queue head's chain three times, 10 to 20 s of network a visit on a busy fleet,
//! only to find that nothing moved (2026-10-28). Now a pass's visits share what it read within
//! 20 s (`repo::READ_REUSE`): a pass over six waiting changes fetches no more than a pass over
//! one. A visit whose operation may run (the head, or a change that needs no check) reads
//! afresh before it claims, so it never builds on a main another runner moved meanwhile.
//!
//! The guards to mutate: `repo.share_reads(true)` in `executor::pass` (`src/executor.rs`):
//! share nothing and the six cost six times the fetches; the `took_shared_read` check after the
//! queue gate in `publish::execute_op` (`src/publish.rs`): drop it and the head builds on the
//! main it read before main moved, and its push is refused once (`main_moved`).

// The cases run a shell adapter, so they run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// Fetches a shared read may still add when a slow box crosses the reuse window once: main,
/// the registry, the queue and the head's chain.
const WINDOW_SLACK: usize = 4;

/// A trunk whose one check wakes on `src/**` only, so a docs change needs no check; runners
/// check `ahead` entries, so waiting entries are visited. A started change that is never
/// submitted holds the queue's head, so every change behind it waits. Returns the box, the
/// work repository and the remote.
fn line() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str("when_paths = [\"src/**\"]\n");
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        // A lease long enough that the idle-head check (two leases) never takes the head that
        // holds the line, an operation that does not exist.
        "[runner]\nahead = 8\nlease_s = 60\nheartbeat_s = 1\n".to_owned(),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/readme.md", "readme\n")],
        &files,
    );
    let (open, _) = test_box.start_and_edit(&repo);
    let handle = bay::repo::Repo::open(&repo).unwrap();
    bay::queue::join(&handle, None, &format!("{open}.1"), &open).unwrap();
    (test_box, repo, remote)
}

/// Submits a change that writes `path`; its id.
fn submit(test_box: &TestBox, repo: &Path, intent: &str, path: &str) -> String {
    let dir = path.split('/').next().unwrap();
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", dir]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), format!("{intent}\n")).unwrap();
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    queued["change"].as_str().unwrap().to_owned()
}

/// The `fetch` spawns of one `bay runner run --once` in `dir`, from bay's own process only, and
/// the pass's result.
fn fetches(test_box: &TestBox, dir: &Path, name: &str) -> (usize, Value) {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let (code, result) = result_json(
        test_box
            .bay(dir)
            .args(["runner", "run", "--once"])
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{result}");
    let text = std::fs::read_to_string(&trace).expect("the trace was written");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let count = lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref() && line["sub"] == "fetch")
        .count();
    (count, result)
}

/// The fetches of a pass over `waiting` changes that only wait behind the head.
fn waiting_pass(waiting: usize) -> usize {
    let (test_box, repo, remote) = line();
    let ids: Vec<String> = (0..waiting)
        .map(|n| {
            submit(
                &test_box,
                &repo,
                &format!("WAIT-{n}"),
                &format!("src/w{n}.rs"),
            )
        })
        .collect();
    let executor = test_box.clone_of(&remote, "executor");
    // The first pass gives each its place; the second only finds them waiting.
    let _ = fetches(&test_box, &executor, "place");
    let (count, result) = fetches(&test_box, &executor, "wait");
    let ran = result["detail"]["ran"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for id in &ids {
        assert!(
            ran.iter()
                .any(|row| row["change"] == id.as_str() && row["decision"] == "queued"),
            "{id} was not visited waiting: {result}"
        );
    }
    count
}

#[test]
fn six_waiting_visits_fetch_no_more_than_one() {
    let one = waiting_pass(1);
    let six = waiting_pass(6);
    assert!(
        six <= one + WINDOW_SLACK,
        "a pass over six waiting changes fetched {six} times, over one {one}"
    );
}

#[test]
fn a_change_that_may_run_reads_main_afresh_before_it_claims() {
    let (test_box, repo, remote) = line();
    let _waits = submit(&test_box, &repo, "WAITS", "src/w.rs");
    let docs = submit(&test_box, &repo, "DOCS", "docs/readme.md");
    let executor = test_box.clone_of(&remote, "executor");
    let _ = fetches(&test_box, &executor, "place");
    // The next pass reads main on the waiting visit, then pauses where the docs change, which
    // needs no check, has passed the queue gate; main moves meanwhile.
    let release = test_box.path().join("release");
    let paused = PathBuf::from(format!("{}.paused", release.display()));
    let runner = test_box
        .bay_process(&executor)
        .args(["--json", "runner", "run", "--once"])
        .env("BAY_TEST_PAUSE_AT", "visit-gated")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let began = Instant::now();
    while !paused.exists() {
        assert!(
            began.elapsed() < Duration::from_secs(120),
            "the runner never reached the gate"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    std::fs::write(repo.join("src/moved.rs"), "fn moved() {}\n").unwrap();
    test_box.git_ok(&repo, &["checkout", "-q", "main"]);
    test_box.git_ok(&repo, &["add", "src/moved.rs"]);
    test_box.git_ok(&repo, &["commit", "-q", "-m", "main moves meanwhile"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "HEAD:main"]);
    std::fs::write(&release, "go").unwrap();
    let out = runner.wait_with_output().unwrap();
    let pass: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    assert!(
        test_box.trunk_change_ids(&remote).contains(&docs),
        "the docs change did not land: {pass}"
    );
    let status = test_box.bay_ok(&repo, &["status", &docs]);
    let row = &status["detail"]["changes"][0]["operation"];
    assert_eq!(
        row["main_moved"], 0,
        "the head built on the main an earlier visit read: {status}"
    );
}

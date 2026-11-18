//! G779 (a fleet finding, 2026-11-18: a runner's visits to changes that only waited in line
//! took 15 to 83 s each, four to eight fetches and up to five pushes, 745 such visits a day on
//! one box: at load one visit outlasted the 20 s a pass's reads stood (G438), so the next visit
//! fetched main, the queue, the registry and the head's chain again). A pass's shared reads now
//! stand as long as it starts operations for a visit that only waits: a pass over six waiting
//! changes, each visit outlasting the old window, fetches and pushes no more than a pass over
//! one. A pre-check that spends a check reads main again first (`Repo::trunk_still`). And the
//! queue head's chain the pass's release check read is the first visit's too: one fetch of it.
//!
//! Guards to mutate: `Repo::reuses` and `Repo::read_chain` (`src/repo.rs`): measure the reads
//! against `read_fresh()` again, and the six visits fetch as six; `pass` (`src/executor.rs`):
//! drop `note_chain`, and the head's chain is fetched twice in a pass.

// The cases run a shell adapter, so they run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

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
        "[runner]\nahead = 8\nlease_s = 60\nheartbeat_s = 30\n".to_owned(),
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
    // The docs change needs no check (G657): this test watches a runner's visit to it.
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    queued["change"].as_str().unwrap().to_owned()
}

/// One `bay runner run --once` in `dir`, every visit outlasting the freshness window: bay's own
/// spawns by subcommand, the arguments of its fetches, and the pass's result.
fn pass(test_box: &TestBox, dir: &Path, name: &str) -> (usize, usize, Vec<String>, Value) {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let (code, result) = result_json(
        test_box
            .bay(dir)
            .args(["runner", "run", "--once"])
            .env("BAY_TEST_READ_FRESH_MS", "0")
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{result}");
    let text = std::fs::read_to_string(&trace).expect("the trace was written");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let own: Vec<&Value> = lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref())
        .collect();
    let count = |sub: &str| own.iter().filter(|line| line["sub"] == sub).count();
    let fetched = own
        .iter()
        .filter(|line| line["sub"] == "fetch")
        .map(|line| line["args"].to_string())
        .collect();
    (count("fetch"), count("push"), fetched, result)
}

/// A pass over `waiting` changes that only wait behind the head: its fetches, pushes and the
/// arguments of its fetches; and the head's change.
fn waiting_pass(waiting: usize) -> (usize, usize, Vec<String>, String) {
    let (test_box, repo, remote) = line();
    let head = bay::queue::Queue::read(&bay::repo::Repo::open(&repo).unwrap())
        .unwrap()
        .head()
        .unwrap()
        .change
        .clone();
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
    let _ = pass(&test_box, &executor, "place");
    let (fetches, pushes, args, result) = pass(&test_box, &executor, "wait");
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
    (fetches, pushes, args, head)
}

#[test]
fn six_waiting_visits_past_the_fresh_window_cost_what_one_does() {
    let (one_fetches, one_pushes, _, _) = waiting_pass(1);
    let (six_fetches, six_pushes, fetched, head) = waiting_pass(6);
    assert!(
        six_fetches <= one_fetches,
        "six waiting visits fetched {six_fetches} times, one {one_fetches}:\n{}",
        fetched.join("\n")
    );
    assert!(
        six_pushes <= one_pushes,
        "six waiting visits pushed {six_pushes} times, one {one_pushes}"
    );
    let head_fetches = fetched
        .iter()
        .filter(|args| args.contains(&format!("refs/bay/changes/{head}:")))
        .count();
    assert_eq!(
        head_fetches,
        1,
        "the head's chain was fetched {head_fetches} times in one pass:\n{}",
        fetched.join("\n")
    );
}

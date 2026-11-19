//! G799 (a fleet measurement, 2026-11-19: in 28 h one box's two runners spent 19,118 s on 1,542
//! visits to a head that waited for a runner of another environment, and 5,882 s on 434 visits
//! to a head another runner held; each fetched main, judged main's red and read the chain again
//! before it found what its pass had already read). A runner's visit that what its pass read
//! already answers ends before it reads anything again:
//! - the queue's head whose checks this box does not serve, while a live runner's box does, is
//!   left to that runner, unclaimed, with no fetch of main of its own;
//! - a head another runner holds, whose lease this looping runner saw beat, is held: its chain is
//!   read again, as the claim reads it (G799b), and the registry and the line are not.
//!
//! Guards to mutate, in `src/publish.rs` `settled_by_what_the_pass_read`: answer `None` for the
//! head (the visit fetches main, judges its red, and only then leaves the head); skip the held
//! branch (each visit fetches the registry, and reads the line again, before its claim finds the
//! lease held).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// A runner's clone of `remote` named `name`, which declares `environment`.
fn runner(test_box: &TestBox, remote: &Path, name: &str, environment: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    test_box.git_ok(&clone, &["config", "bay.checkEnvironment", environment]);
    clone
}

/// One `bay runner run --once` in `dir`, every visit outlasting the pass's shared reads (as at
/// load): its result, and how often it fetched main.
fn pass(test_box: &TestBox, dir: &Path, name: &str) -> (Value, usize) {
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
    let main = lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref() && line["sub"] == "fetch")
        .filter(|line| line["args"].to_string().contains("refs/heads/main"))
        .count();
    (result, main)
}

#[test]
fn a_head_this_box_cannot_serve_is_left_with_no_fetch_of_main() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    // One required check, in env-b, on src/ only: a docs change needs none.
    let files: Vec<(String, String)> =
        check_files(&test_box.relay("true"), &passing_result(1), 1, 60)
            .into_iter()
            .map(|(path, text)| {
                let text = text.replace("\"local\"", "\"env-b\"");
                if path == ".bay/policy.toml" {
                    (path, format!("{text}when_paths = [\"src/**\"]\n"))
                } else {
                    (path, text)
                }
            })
            .chain([(
                ".bay/config.toml".to_owned(),
                "[runner]\nlease_s = 60\nheartbeat_s = 1\n".to_owned(),
            )])
            .collect();
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
        &files,
    );

    // GIVEN a live runner whose box serves env-b (its presence names a cadence of five minutes,
    // so it reads as live however slowly this box runs the test), and a box that serves only
    // env-a
    let serving = runner(&test_box, &remote, "exec-b", "env-b");
    let other = runner(&test_box, &remote, "exec-a", "env-a");
    let published = test_box.bay_ok(
        &serving,
        &["runner", "run", "--once", "--interval-s", "300"],
    );
    assert_eq!(
        published["detail"]["presence"]["pushed"], true,
        "{published}"
    );
    // AND a line of a docs change, which needs no check, then a change whose check runs in env-b
    let handle = bay::repo::Repo::open(&repo).unwrap();
    let submit = |intent: &str, path: &str| {
        let dir = path.split('/').next().unwrap();
        let started = test_box.bay_ok(&repo, &["start", intent, "--paths", &format!("{dir}/")]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(path), format!("{intent}\n")).unwrap();
        let (code, queued) = result_json(
            test_box
                .bay(&view)
                .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
                .arg("merge"),
        );
        assert_eq!(code, 0, "{queued}");
        let op = queued["op_id"].as_str().unwrap().to_owned();
        let id = queued["change"].as_str().unwrap().to_owned();
        bay::queue::join(&handle, None, &op, &id).unwrap();
        (id, op)
    };
    let (docs, _) = submit("DOCS", "docs/b.md");
    let (_, op) = submit("CODE", "src/b.rs");

    // WHEN the box that cannot serve env-b lands the docs change, and its pass reaches the new
    // head with what it read before the landing forgotten
    let (left, fetched) = pass(&test_box, &other, "leave");
    assert!(test_box.trunk_change_ids(&remote).contains(&docs), "{left}");

    // THEN it leaves the head to the serving runner with no fetch of main for that visit
    let row = left["detail"]["ran"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["op_id"] == op.as_str()))
        .cloned()
        .unwrap_or_default();
    assert_eq!(row["reason"], "CHECK_UNAVAILABLE", "{left}");
    assert_eq!(fetched, 1, "the visit read main again: {left}");
}

/// How often the process that wrote first to the trace at `path` fetched anything, fetched the
/// registry, and fetched every chain (once a pass).
fn traced(path: &Path) -> (usize, usize, usize) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let fetches: Vec<String> = lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref() && line["sub"] == "fetch")
        .map(|line| line["args"].to_string())
        .collect();
    let registry = fetches
        .iter()
        .filter(|args| args.contains("refs/bay/registry"))
        .count();
    let passes = fetches
        .iter()
        .filter(|args| args.contains("refs/bay/changes/*"))
        .count();
    (fetches.len(), registry, passes)
}

#[test]
fn a_looping_runner_reads_main_no_more_for_a_head_another_runner_holds() {
    let line = support::queued::host(&support::queued::Check {
        config: "[runner]\nlease_s = 60\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let head = line.queue("slow", "src");
    let a = line.executor("exec-a", support::PRODUCER);
    let b = line.executor("exec-b", support::queued::OTHER_PRODUCER);
    line.test_box
        .git_ok(&b, &["config", "bay.runnerName", "exec-b"]);

    // GIVEN runner A holds the head in its check
    let holder = line.spawn(&a);
    line.wait_held();

    // WHEN runner B loops beside it for a few passes
    let trace = line.test_box.path().join("looping-trace.jsonl");
    let journal = line.test_box.path().join("looping-journal");
    let mut looping = line
        .test_box
        .bay_process(&b)
        .args(["--json", "runner", "run", "--interval-s", "1"])
        .env("BAY_TEST_GIT_TRACE", &trace)
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&journal).unwrap())
        .spawn()
        .unwrap();
    let began = std::time::Instant::now();
    let visits = || {
        std::fs::read_to_string(&journal)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(&format!("{head}.1")) && line.contains("] ok ("))
            .count()
    };
    while visits() < 3 && began.elapsed() < std::time::Duration::from_secs(90) {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let _ = looping.kill();
    let _ = looping.wait();
    line.go();
    let _ = support::queued::finished(holder);

    // THEN each of its visits to the held head found it held without the reads the claim makes
    let (fetches, registry, passes) = traced(&trace);
    assert!(fetches > 0, "runner B fetched nothing");
    let seen = visits();
    let lines = std::fs::read_to_string(&journal).unwrap_or_default();
    assert!(
        seen >= 3,
        "runner B visited the held head {seen} times: {lines}"
    );
    assert!(
        registry < seen,
        "runner B fetched the registry {registry} times in {passes} passes, for {seen} visits to the held head"
    );
}

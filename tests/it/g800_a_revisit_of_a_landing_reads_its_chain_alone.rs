//! G800 (a fleet measurement, 2026-11-19: in 28 h one box's two runners made 524 visits to an
//! operation another runner had claimed and landed a median 59 s earlier, 10,353 s in all; each
//! fetched main and the registry, scanned main for the landing, fetched the logs for a terminal
//! record the landing runner had written, and pushed a chain it had not changed). A runner's
//! visit to an operation whose place the line released `published` reads that change's chain
//! alone: it answers `merged`, as a scan of main would, with no fetch of the logs and no push;
//! and a looping runner never answers that another runner holds it.
//!
//! Each case holds runner B at the start of its first visit, a change that has no place yet,
//! while runner A lands the head; B's pass then reaches the head on the chain it read before the
//! landing, and on a line it reads again after that first visit (G788).
//!
//! Guards to mutate, in `src/publish.rs` `settled_by_what_the_pass_read`: drop the `published`
//! release test (the first case: the revisit reads the pass's stale line and answers it queued,
//! or fetches main and the logs and pushes the chain); drop it and the chain's read before the
//! lease (G799b) both (the second case: a looping runner answers the lease held). Either one alone
//! keeps the second case's answer.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::queued::{Check, Held, OTHER_PRODUCER, Trust, finished, host_under, ran_for};
use support::{PRODUCER, check_files, passing_result, result_json};

/// A line whose head runner A holds in its check (run 1 waits for the test), a docs change with
/// no place yet, and runner B's clone. Returns the host, the head, and runners A's process and
/// B's clone.
fn line() -> (Held, String, std::process::Child, PathBuf) {
    // One pre-merge check, on src/ only: a docs change needs none.
    let policy = check_files("x", &passing_result(1), 1, 120)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| text.replacen("timeout_s", "when_paths = [\"src/**\"]\ntimeout_s", 1))
        .unwrap();
    let host = host_under(
        &Check {
            config: "[runner]\nlease_s = 60\nheartbeat_s = 1\nahead = 4\n",
            policy: Some(&policy),
            seed: &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
            hold_first: true,
            fails: "false",
        },
        Trust::Unsigned,
    );
    let head = host.queue("head", "src");
    let a = host.executor("exec-a", PRODUCER);
    let b = host.executor("exec-b", OTHER_PRODUCER);
    host.test_box
        .git_ok(&b, &["config", "bay.runnerName", "exec-b"]);
    let holder = host.spawn(&a);
    host.wait_held();
    // A docs change, submitted and not yet placed: runner B's pass visits it first.
    let started = host
        .test_box
        .bay_ok(&host.repo, &["start", "notes", "--paths", "docs/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("docs/notes.md"), "notes\n").unwrap();
    let (code, queued) = result_json(
        host.test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    (host, head, holder, b)
}

/// Starts runner B in `clone` with `args`, held at the start of its first visit until
/// `go` exists, its Git calls traced to `trace` and its journal to `journal`. `go` is B's own: the
/// host's ([`Held::go`]) lets runner A's check go, and B's visit must not start with it (a gate
/// finding, 2026-11-20: B's visit raced A's landing, and rightly answered the head held).
fn held_runner(
    host: &Held,
    clone: &Path,
    args: &[&str],
    (go, trace, journal): (&Path, &Path, &Path),
) -> std::process::Child {
    let child = host
        .test_box
        .bay_process(clone)
        .args(args)
        .env("BAY_TEST_PAUSE_AT", "visit-begins")
        .env("BAY_TEST_PAUSE_FILE", go)
        .env("BAY_TEST_GIT_TRACE", trace)
        .stdout(std::process::Stdio::piped())
        .stderr(std::fs::File::create(journal).unwrap())
        .spawn()
        .unwrap();
    let paused = go.with_extension("paused");
    let began = Instant::now();
    while !paused.exists() {
        assert!(
            began.elapsed() < Duration::from_secs(120),
            "runner B never began a visit"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    child
}

/// The Git calls of the process that wrote first to `trace`: (subcommand, arguments).
fn calls(trace: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(trace).unwrap_or_default();
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref())
        .map(|line| {
            (
                line["sub"].as_str().unwrap_or_default().to_owned(),
                line["args"].to_string(),
            )
        })
        .collect()
}

#[test]
fn a_revisit_of_a_landing_reads_its_chain_alone_and_answers_merged() {
    let (host, head, holder, b) = line();
    let dir = host.test_box.path();
    let (go, trace, journal) = (dir.join("b-go"), dir.join("trace"), dir.join("journal"));
    // GIVEN runner B's pass read the head's chain, and waits at its first visit
    let runner = held_runner(
        &host,
        &b,
        &["--json", "runner", "run", "--once"],
        (&go, &trace, &journal),
    );
    // WHEN runner A lands the head meanwhile
    host.go();
    let landing = finished(holder);
    let landed = ran_for(&landing, &head).clone();
    assert_eq!(landed["decision"], "merged", "{landing}");
    std::fs::write(&go, "go").unwrap();
    let pass = finished(runner);

    // THEN runner B's visit answers merged, as runner A's landing did
    let row = ran_for(&pass, &head);
    assert_eq!(row["decision"], "merged", "{pass}");
    assert_eq!(row["detail"]["existing"], true, "{pass}");
    let mut found = row["detail"].clone();
    found.as_object_mut().unwrap().remove("existing");
    assert_eq!(found, landed["detail"], "{pass}");
    // AND it read the head's chain, fetched no log and pushed nothing of the head's
    let calls = calls(&trace);
    let chain = format!("refs/bay/changes/{head}");
    let logs: Vec<&(String, String)> = calls
        .iter()
        .filter(|(sub, args)| sub == "fetch" && args.contains("refs/bay/log/"))
        .collect();
    assert!(logs.is_empty(), "runner B fetched the logs: {logs:?}");
    let pushed: Vec<&(String, String)> = calls
        .iter()
        .filter(|(sub, args)| sub == "push" && args.contains(&chain))
        .collect();
    assert!(
        pushed.is_empty(),
        "runner B pushed the head's chain: {pushed:?}"
    );
}

#[test]
fn a_looping_runner_never_answers_held_for_an_operation_that_landed() {
    let (host, head, holder, b) = line();
    let dir = host.test_box.path();
    let (go, trace, journal) = (dir.join("b-go"), dir.join("trace"), dir.join("journal"));
    // GIVEN a looping runner B whose pass read the head held by runner A, and waits at its first
    // visit
    let mut runner = held_runner(
        &host,
        &b,
        &["--json", "runner", "run", "--interval-s", "1"],
        (&go, &trace, &journal),
    );
    // WHEN runner A lands the head meanwhile
    host.go();
    let _ = finished(holder);
    std::fs::write(&go, "go").unwrap();

    // THEN runner B's visit answers merged, never that runner A holds it
    let line = |text: &str| {
        text.lines()
            .find(|line| line.contains(&format!("{head}.1 [")))
            .map(str::to_owned)
    };
    let began = Instant::now();
    let seen = loop {
        let text = std::fs::read_to_string(&journal).unwrap_or_default();
        if let Some(seen) = line(&text) {
            break seen;
        }
        assert!(
            began.elapsed() < Duration::from_secs(120),
            "runner B never visited the head: {text}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = runner.kill();
    let _ = runner.wait();
    assert!(
        seen.contains("] merged"),
        "runner B answered for a landed head: {seen}"
    );
}

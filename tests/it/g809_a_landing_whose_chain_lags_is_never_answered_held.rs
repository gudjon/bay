//! G809 (a finding, 2026-11-20: a landing pushes main and the line's `published` release first,
//! and its chain's record after; a runner that read the chain between the two answered the landed
//! head held, a lease long when the record's push failed). A head whose chain still says checked,
//! while the line released its place `published`, is never answered held: the visit reads the
//! line once more in that window alone, and answers from the chain, with main fetched once for
//! the landing commit, or the long way.
//!
//! Each case holds runner B at the start of its visit to the head, its reads of the line and the
//! chain from before the landing, while runner A lands it and stops right after its publishing
//! push (`BAY_TEST_KILL_AT=after-publish`), its records kept apart from that push
//! (`BAY_TEST_LAND_APART`, as a batch's landing pushes them): the chain says checked, held by
//! A, and only the line says it landed. A looping B answers on its short path (G799); a `--once`
//! B, which watches no lease across passes, at its claim.
//!
//! Guard to mutate, in `src/publish.rs`: `after_release` answering `Stands` (the looping case
//! answers the landed head held; the `--once` case is answered at its claim by a scan of a main it
//! fetched, as before G809, and stays a check that the claim never answers it held).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use support::PRODUCER;
use support::queued::{Check, Held, OTHER_PRODUCER, Trust, host_under};

/// A line whose head runner A holds in its check (run 1 waits for the test), A stopping right
/// after its publishing push, and runner B's clone. Returns the host, the head, A's process and
/// B's clone.
fn line() -> (Held, String, std::process::Child, PathBuf) {
    let host = host_under(
        &Check {
            config: "[runner]\nlease_s = 60\nheartbeat_s = 1\n",
            policy: None,
            seed: &[("src/a.rs", "fn a() {}\n")],
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
    let holder = host
        .test_box
        .bay_process(&a)
        .args(["--json", "runner", "run", "--once"])
        .env("BAY_TEST_KILL_AT", "after-publish")
        .env("BAY_TEST_LAND_APART", "1")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    host.wait_held();
    (host, head, holder, b)
}

/// Runner B in `clone` with `args`, held at the start of its first visit until `go` exists, its
/// journal to `journal`.
fn held_runner(
    host: &Held,
    clone: &Path,
    args: &[&str],
    (go, journal): (&Path, &Path),
) -> std::process::Child {
    let child = host
        .test_box
        .bay_process(clone)
        .args(args)
        .env("BAY_TEST_PAUSE_AT", "visit-begins")
        .env("BAY_TEST_PAUSE_FILE", go)
        .stdout(std::process::Stdio::null())
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

/// Lets runner A land the head and stop before its chain records it, then runner B's visit go;
/// returns B's first journal line for the head.
fn answer(args: &[&str]) -> String {
    let (host, head, mut holder, b) = line();
    let dir = host.test_box.path().to_path_buf();
    let (go, journal) = (dir.join("b-go"), dir.join("b-journal"));
    // GIVEN runner B's visit to the head began on reads from before the landing
    let mut runner = held_runner(&host, &b, args, (&go, &journal));
    // WHEN runner A lands the head and stops before its chain records the landing
    host.go();
    let _ = holder.wait();
    assert!(
        host.test_box.trunk_change_ids(&host.remote).contains(&head),
        "runner A did not publish the head"
    );
    std::fs::write(&go, "go").unwrap();
    let began = Instant::now();
    let seen = loop {
        let text = std::fs::read_to_string(&journal).unwrap_or_default();
        if let Some(line) = text
            .lines()
            .find(|line| line.contains(&format!("{head}.1 [")))
        {
            break line.to_owned();
        }
        assert!(
            began.elapsed() < Duration::from_secs(120),
            "runner B never answered for the head: {text}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = runner.kill();
    let _ = runner.wait();
    seen
}

#[test]
fn a_looping_runner_answers_a_landing_whose_chain_lags_merged() {
    let seen = answer(&["--json", "runner", "run", "--interval-s", "1"]);
    // THEN runner B answers merged, never that runner A holds it
    assert!(seen.contains("] merged"), "runner B answered: {seen}");
}

#[test]
fn a_once_runner_answers_a_landing_whose_chain_lags_merged_at_its_claim() {
    let seen = answer(&["--json", "runner", "run", "--once"]);
    // THEN runner B answers merged, never that runner A holds it
    assert!(seen.contains("] merged"), "runner B answered: {seen}");
}

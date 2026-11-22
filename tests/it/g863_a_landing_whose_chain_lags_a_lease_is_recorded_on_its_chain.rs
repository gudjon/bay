//! G863 (a fleet finding, 2026-11-22: a batch member's chain still said `checked` 3.7 h after its
//! landing, and two runners visited it about once a minute each, about 320 visits, each answered
//! landed with nothing written): a landing whose chain still says checked a lease after the line
//! released its place `published` is one its landing runner will not record. The looping runner
//! that finds it so records the landing on its chain, from main, and visits it no more.
//!
//! Runner A lands the head and stops right after its publishing push
//! (`BAY_TEST_KILL_AT=after-publish`): the chain says checked, held by A, and only the line and
//! main say it landed. Looping runner B then visits it.
//!
//! Guard to mutate, in `src/publish.rs` `chain_left_checked`: return `false` (B answers the
//! landing at each visit and the chain says checked for ever).

#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};

use serde_json::Value;

use support::PRODUCER;
use support::queued::{Check, Held, OTHER_PRODUCER, Trust, host_under};

#[test]
fn a_landing_whose_chain_lags_a_lease_is_recorded_on_its_chain() {
    let host = host_under(
        &Check {
            config: "[runner]\nlease_s = 2\nheartbeat_s = 1\n",
            policy: None,
            seed: &[("src/a.rs", "fn a() {}\n")],
            hold_first: false,
            fails: "false",
        },
        Trust::Unsigned,
    );
    let head = host.queue("head", "src");
    let a = host.executor("exec-a", PRODUCER);
    let b = host.executor("exec-b", OTHER_PRODUCER);
    host.test_box
        .git_ok(&b, &["config", "bay.runnerName", "exec-b"]);
    // GIVEN runner A landed the head and stopped before its chain recorded the landing
    let _ = host
        .test_box
        .bay_process(&a)
        .args(["--json", "runner", "run", "--once"])
        .env("BAY_TEST_KILL_AT", "after-publish")
        .output()
        .unwrap();
    assert!(
        host.test_box.trunk_change_ids(&host.remote).contains(&head),
        "runner A did not publish the head"
    );
    assert_eq!(op_state(&host, &head), "checked");

    // WHEN looping runner B visits it for longer than a lease
    let journal = host.test_box.path().join("b-journal");
    let mut runner = host
        .test_box
        .bay_process(&b)
        .args(["--json", "runner", "run", "--interval-s", "1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&journal).unwrap())
        .spawn()
        .unwrap();
    let began = Instant::now();
    while op_state(&host, &head) == "checked" && began.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(200));
    }
    let recorded = host_record(&host, &head);
    let visits = || {
        std::fs::read_to_string(&journal)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(&format!("{head}.1 [")))
            .count()
    };
    // the visit that recorded the landing writes its journal line after its write
    std::thread::sleep(Duration::from_secs(2));
    let after_recording = visits();
    std::thread::sleep(Duration::from_secs(4));
    let later = visits();
    let lines = std::fs::read_to_string(&journal).unwrap_or_default();
    let _ = runner.kill();
    let _ = runner.wait();

    // THEN the chain records the landing
    assert_eq!(
        recorded["operations"][0]["state"], "published",
        "{recorded}"
    );
    assert_eq!(recorded["state"], "landed", "{recorded}");
    // AND runner B visits it no more
    assert_eq!(
        after_recording, later,
        "runner B kept visiting the landed head:\n{lines}"
    );
}

/// The change record at the host's head of change `id`'s chain.
fn host_record(host: &Held, id: &str) -> Value {
    let text = host.test_box.git_bare(
        &host.remote,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    serde_json::from_str(&text).unwrap()
}

/// The state of change `id`'s first operation at the host.
fn op_state(host: &Held, id: &str) -> String {
    host_record(host, id)["operations"][0]["state"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

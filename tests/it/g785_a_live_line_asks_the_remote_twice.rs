//! G785 (a measurement, 2026-11-18: a live `bay queue` asked the remote five times, the line's
//! refs, three batches of its changes' chains and the list of merge requests, 8.9 s of 10 s on
//! the wire). A live read of the line asks the remote twice, one listing and one fetch, however
//! many changes are in line; the line it answers is the same. Read from the trace a test build
//! writes (G171).
//!
//! Guard to mutate: in `src/line.rs` `read`, answer `None` from `Line::read`, and the read asks
//! as before: once for the line, once per batch of chains, once for the requests.

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::queued::{Check, host};

/// The calls to the remote one `bay queue` made in `clone`, and its answer.
fn asked(test_box: &support::TestBox, clone: &Path, name: &str) -> (usize, Value) {
    let trace = test_box.path().join(format!("{name}.jsonl"));
    let _ = std::fs::remove_file(&trace);
    let (code, line) = support::result_json(
        test_box
            .bay(clone)
            .args(["queue"])
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{line}");
    let text = std::fs::read_to_string(&trace).unwrap_or_default();
    let calls = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| matches!(line["sub"].as_str(), Some("fetch" | "ls-remote" | "push")))
        .count();
    (calls, line)
}

#[test]
fn a_live_read_of_the_line_asks_the_remote_twice_however_long_the_line() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let observer = held.test_box.clone_of(&held.remote, "observer");

    // GIVEN three changes in line WHEN another clone reads the line THEN it asks twice
    let first: Vec<String> = ["one", "two", "three"]
        .iter()
        .map(|file| held.queue(file, "src"))
        .collect();
    let (calls, line) = asked(&held.test_box, &observer, "three");
    assert_eq!(line["detail"]["depth"], 3, "{line}");
    assert_eq!(line["detail"]["unread"], serde_json::json!([]), "{line}");
    assert!(
        calls <= 2,
        "a line of three asked the remote {calls} times: {line}"
    );

    // AND with nine in line it still asks twice, and names them all head first
    let more: Vec<String> = ["four", "five", "six", "seven", "eight", "nine"]
        .iter()
        .map(|file| held.queue(file, "src"))
        .collect();
    let (calls, line) = asked(&held.test_box, &observer, "nine");
    let shown: Vec<&str> = line["detail"]["places"]
        .as_array()
        .unwrap()
        .iter()
        .map(|place| place["change"].as_str().unwrap())
        .collect();
    let all: Vec<String> = first.into_iter().chain(more).collect();
    assert_eq!(shown, all, "{line}");
    assert_eq!(line["detail"]["unread"], serde_json::json!([]), "{line}");
    assert!(
        calls <= 2,
        "a line of nine asked the remote {calls} times: {line}"
    );
}

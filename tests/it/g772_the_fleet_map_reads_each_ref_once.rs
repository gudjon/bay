//! G772 (owner, 2026-11-17: a panel sat on "Reading bay…"; one `bay status --all` of a busy
//! repository ran 46 Git commands, 19 of them the same registry read and 5 the same trunk read).
//! The fleet map moves no ref, so it reads each ref once: no one-ref read repeats within it. Read
//! from the trace a test build writes (G171).
//!
//! Guard to mutate: in `src/fleet.rs` `run`, drop `ref_reads_kept`, and the map reads the trunk
//! again.

use crate::support;

use std::collections::BTreeMap;

use serde_json::Value;
use support::{TestBox, unchecked_files};

#[test]
fn the_fleet_map_reads_no_ref_twice() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    // GIVEN changes on this machine, and a status read that fetched the fleet
    for n in 0..3 {
        test_box.bay_ok(&repo, &["start", &format!("CARD-{n}"), "--paths", "src/"]);
    }
    test_box.bay_ok(&repo, &["status"]);

    // WHEN the fleet map is read
    let trace = test_box.path().join("fleet-trace.jsonl");
    let (code, map) = support::result_json(
        test_box
            .bay(&repo)
            .args(["status", "--all"])
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{map}");

    // THEN no one-ref read ran twice
    let text = std::fs::read_to_string(&trace).unwrap_or_default();
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let mut reads: BTreeMap<String, usize> = BTreeMap::new();
    for line in &lines {
        let args = line["args"].as_array().cloned().unwrap_or_default();
        let words: Vec<&str> = args.iter().filter_map(Value::as_str).collect();
        if let Some(at) = words.iter().position(|word| *word == "rev-parse")
            && words.get(at + 1) == Some(&"--verify")
            && words.get(at + 2) == Some(&"-q")
        {
            *reads.entry(words[at + 3..].join(" ")).or_default() += 1;
        }
    }
    assert!(!reads.is_empty(), "the trace holds no ref read: {text}");
    let again: Vec<_> = reads.iter().filter(|(_, n)| **n > 1).collect();
    assert!(again.is_empty(), "read more than once: {again:?}");
}

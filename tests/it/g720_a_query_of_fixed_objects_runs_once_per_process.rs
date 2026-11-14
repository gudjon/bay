//! G720 (a measurement, 2026-11-14: one `bay status` ran the same history query on the same
//! object ids two and three times). A query that reads only objects named for good (`log`,
//! `rev-list`, `merge-base`, `diff-tree` or `ls-tree` over object ids, with no option that reads
//! refs, the clock, signature keys, notes or mailmaps) runs once per process: its answer cannot
//! change, so a second ask is answered from the first.
//!
//! Guard to mutate: `src/git.rs` `fixed_query`: answer false (each ask runs Git again).

#![cfg(unix)]

use crate::support;

use std::collections::BTreeMap;

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

const FIXED: [&str; 5] = ["log", "rev-list", "merge-base", "diff-tree", "ls-tree"];

#[test]
fn a_listing_runs_each_query_of_fixed_objects_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let _ = test_box.start_and_edit(&repo);
    let _ = test_box.start_and_edit(&repo);

    // GIVEN a sweep the next read is served by, so that read runs in its process alone
    let (code, swept) = result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 0, "{swept}");
    support::sweeps_ended(&repo);

    // WHEN a listing reads the store whole
    let trace = test_box.path().join("trace.jsonl");
    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .arg("status")
            .env("BAY_TEST_SAVED", "off")
            .env("BAY_TEST_SWEPT_S", "600")
            // Answers another process kept (G721) would hide what this one asks.
            .env("BAY_TEST_FACTS", "off")
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{result}");
    support::sweeps_ended(&repo);

    // THEN no query of the kinds that read fixed objects ran twice with the same arguments in
    // one process
    let mut asked: BTreeMap<(u64, String), usize> = BTreeMap::new();
    for line in std::fs::read_to_string(&trace).unwrap_or_default().lines() {
        let Ok(line) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let sub = line["sub"].as_str().unwrap_or_default();
        if !FIXED.contains(&sub) || !line["input"].is_null() {
            continue;
        }
        let args: Vec<&str> = line["args"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let pid = line["pid"].as_u64().unwrap_or_default();
        *asked.entry((pid, args.join(" "))).or_default() += 1;
    }
    let again: Vec<_> = asked.iter().filter(|(_, n)| **n > 1).collect();
    assert!(
        !asked.is_empty(),
        "the listing ran no query of fixed objects"
    );
    assert!(again.is_empty(), "asked again in one process: {again:#?}");
}

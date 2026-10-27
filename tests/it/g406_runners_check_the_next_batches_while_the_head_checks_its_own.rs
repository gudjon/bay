//! G406 (owner, 2026-10-27: "go for speculative batch pipelining"; a fleet finding the same
//! night: one batch was checked at a time while runners spent about half their time
//! pre-checking single entries the batch would check again). While the head checks its batch,
//! a runner that does not hold it checks the next batch, the one the head of the line behind
//! will form, on the main the head's batch predicts. When that batch's turn comes and main is
//! what was predicted, it lands on that record, with no run of its own.
//!
//! Guard to mutate, in `src/publish.rs`: `precheck` routing to `speculate_batch` when batching
//! is on (route to the per-entry pre-check instead).

#![cfg(unix)]

use crate::support;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};

const CONFIG: &str =
    "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 4\nbatch = { min = 2, max = 2 }\n";

#[test]
fn the_next_batch_is_checked_ahead_and_lands_with_no_run_of_its_own() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let ids = [
        held.queue("one", "src"),
        held.queue("two", "src"),
        held.queue("three", "src"),
        held.queue("four", "src"),
    ];
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    // A holds the head, and its batch's check waits.
    let runner_a = held.spawn(&a);
    held.wait_held();
    // B checks the next batch, three and four, on the main one and two predict.
    let ahead = held.pass(&b);
    assert_eq!(held.requests().len(), 2, "one held run, one ahead: {ahead}");
    let spoken = &held.requests()[1];
    let mut changed: Vec<&str> = spoken["changed_paths"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    changed.sort_unstable();
    assert_eq!(changed, ["src/four.rs", "src/three.rs"], "{spoken}");
    held.go();
    let pass = finished(runner_a);
    let landed = held.test_box.trunk_change_ids(&held.remote);
    for id in &ids {
        assert!(landed.contains(id), "{id} did not land: {pass}");
    }
    assert_eq!(
        held.requests().len(),
        2,
        "the second batch ran again at its turn: {pass}"
    );
}

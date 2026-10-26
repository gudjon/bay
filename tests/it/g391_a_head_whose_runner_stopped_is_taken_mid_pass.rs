//! G391 (F13; a fleet finding, 2026-10-26: a runner hung 19 minutes holding the queue head while
//! the other runner was busy pre-checking): a runner takes a head whose holder stopped, between
//! two of its own operations, not at its next pass's start.
//!
//! Runner A holds the head in a long check and is then killed. Runner B, which found the head
//! held by a live runner at its pass's start and went on to pre-check the entries behind it,
//! takes the head over once A's chain has been silent for two leases, in the same pass.
//!
//! Guard to mutate, in `src/executor.rs`: `abandoned_head` returning `None` (B's pass ends with
//! the head still A's).

#![cfg(unix)]

use crate::support;

use std::time::Duration;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};

const CONFIG: &str = "[runner]\nlease_s = 2\nheartbeat_s = 1\nahead = 2\n";

#[test]
fn a_runner_takes_a_stopped_runners_head_between_its_own_operations() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        // Every run but the held first takes a while, so B is mid-pass when A stops.
        fails: "sleep 6; false",
    });
    let one = held.queue("one", "src");
    let _two = held.queue("two", "src");
    let _three = held.queue("three", "src");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let mut runner_a = held.spawn(&a);
    held.wait_held();
    let runner_b = held.spawn(&b);
    // B finds the head held by a live A, and goes on to pre-check the entries behind it.
    std::thread::sleep(Duration::from_secs(4));
    runner_a.kill().unwrap();
    let _ = runner_a.wait();
    let pass = finished(runner_b);
    let took = pass["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["change"] == one.as_str() && row["decision"] == "merged");
    assert!(took, "B left the stopped runner's head in its pass: {pass}");
}

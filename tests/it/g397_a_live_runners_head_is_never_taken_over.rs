//! G397 (a fleet finding, 2026-10-26: a runner held the head through a long copy, wrote nothing
//! to its chain for three minutes, and the other runner took the head over and began the same
//! work again, so every long head ran twice). A runner takes over a head only once its holder's
//! presence is quiet too, never because its chain is.
//!
//! Guard to mutate, in `src/executor.rs`: `holder_alive` returning `false`.

#![cfg(unix)]

use crate::support;

use std::time::Duration;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};

const CONFIG: &str = "[runner]\nlease_s = 2\nheartbeat_s = 30\nahead = 2\n";

#[test]
fn a_head_whose_holder_still_beats_is_left_to_it() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        // Every run but the held first takes a while, so B is mid-pass long after A's chain
        // went quiet.
        fails: "sleep 6; false",
    });
    let one = held.queue("one", "src");
    let _two = held.queue("two", "src");
    let _three = held.queue("three", "src");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let runner_a = held.spawn(&a);
    held.wait_held();
    let runner_b = held.spawn(&b);
    std::thread::sleep(Duration::from_secs(4));
    let pass = finished(runner_b);
    let took = pass["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["change"] == one.as_str() && row["detail"]["lease"] != "held");
    assert!(
        !took,
        "B took the head of a runner that still beats: {pass}"
    );
    held.go();
    let _ = finished(runner_a);
}

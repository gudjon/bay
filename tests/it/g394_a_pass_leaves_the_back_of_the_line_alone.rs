//! G394 (a fleet finding, 2026-10-26: a runner's pass visited every one of 130 waiting changes,
//! at several network round trips each, only to find each still waiting; a pass took twenty
//! minutes, and a change with real work waited for the pass to reach it). A pass works only on
//! what can move: the head, the entries checked ahead of their turn and one batch. A change that
//! only waits further back is left for a later pass, which the line reaches.
//!
//! Guard to mutate, in `src/executor.rs`: `within_reach` returning `true` for every operation.

#![cfg(unix)]

use crate::support;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};

#[test]
fn a_pass_leaves_changes_far_back_in_the_line_alone() {
    let held = host(&Check {
        config: "[runner]\nahead = 0\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let _one = held.queue("one", "src");
    let _two = held.queue("two", "src");
    let three = held.queue("three", "src");
    let four = held.queue("four", "src");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let runner_a = held.spawn(&a);
    held.wait_held();
    // With the head held, nothing moves the line, so B has nothing to do for the back of it.
    let pass = held.pass(&b);
    let ran = pass["detail"]["ran"].as_array().unwrap();
    for far in [&three, &four] {
        assert!(
            ran.iter().all(|row| row["change"] != far.as_str()),
            "the pass visited {far}, which waits at the back of the line: {pass}"
        );
    }
    held.go();
    let _ = finished(runner_a);
}

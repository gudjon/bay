//! G552 (a fleet finding, 2026-10-29: one head yielded "main moved" pass after pass while changes
//! that need no check landed past it, one during each of its checks). Once main moved under the
//! head, a change that needs no check keeps its place until the head lands.
//!
//! G403 runs such a change from its own place while the head still runs. Each landing moved main
//! under the head, its push was refused, and it checked again, so the head paid one check per
//! change that passed it. Now one move costs the head one check.
//!
//! Guard to mutate, in `src/publish.rs`: `main_moved_under` returning `false` (the second document
//! lands while the head checks again).

#![cfg(unix)]

use crate::support;

use std::time::Duration;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};
use support::{check_files, passing_result};

#[test]
fn a_document_waits_for_a_head_that_main_moved_under() {
    // One pre-merge check, required only for src/.
    let policy = check_files("x", &passing_result(1), 1, 120)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| text.replacen("timeout_s", "when_paths = [\"src/**\"]\ntimeout_s", 1))
        .unwrap();
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 4\n",
        policy: Some(&policy),
        seed: &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
        hold_first: true,
        fails: "false",
    });
    let slow = held.queue("slow", "src");
    let first = held.queue_with("first", &["docs"], &[("docs/first.md", "first\n")]);
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    // The head's check on the main the first document moved waits too.
    held.hold_run(2);
    let runner_a = held.spawn(&a);
    held.wait_held();
    std::thread::sleep(Duration::from_secs(1));
    let pass = held.pass(&b);
    assert!(
        held.test_box
            .trunk_change_ids(&held.remote)
            .contains(&first),
        "the first document waited behind a head main had not moved under: {pass}"
    );
    held.go();
    held.wait_held_run(2);
    let second = held.queue_with("second", &["docs"], &[("docs/second.md", "second\n")]);
    let pass = held.pass(&b);
    assert!(
        !held
            .test_box
            .trunk_change_ids(&held.remote)
            .contains(&second),
        "the second document moved main under the head again: {pass}"
    );
    held.go_run(2);
    let ran = finished(runner_a);
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&slow),
        "the head did not land on its second check: {ran}"
    );
    let pass = held.pass(&b);
    assert!(
        held.test_box
            .trunk_change_ids(&held.remote)
            .contains(&second),
        "the second document never landed: {pass}"
    );
}

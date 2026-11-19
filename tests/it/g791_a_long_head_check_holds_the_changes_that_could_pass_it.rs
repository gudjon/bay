//! G791 (owner ruling 2026-11-19, "hold passers on long checks"; fleet findings: overnight 21 of
//! 207 landings were checked again because main moved, nearly all 10-minute heads checked two to
//! four times while changes that passed them landed; the next morning an 18-minute head ran
//! three times while 13 changes waited). Once the line's head has been checked for the hold
//! window, a change that needs no check keeps its place: it lands right after the head, and the
//! head lands on its first check.
//!
//! The window is two minutes; a test build reads it from `bay.testPasserHoldS`, here zero, so the
//! head is under a long check from its claim.
//!
//! Guard to mutate, in `src/publish.rs`: `head_check_long` returning `false` (the document lands
//! while the head checks, main moves under it, and it checks again).

#![cfg(unix)]

use crate::support;

use std::time::Duration;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};
use support::{check_files, passing_result};

#[test]
fn a_document_waits_behind_a_head_under_a_long_check() {
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
    let doc = held.queue_with("doc", &["docs"], &[("docs/doc.md", "doc\n")]);
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    for clone in [&a, &b] {
        held.test_box
            .git_ok(clone, &["config", "bay.testPasserHoldS", "0"]);
    }

    // The head is under its check; the document cannot pass it.
    let runner_a = held.spawn(&a);
    held.wait_held();
    std::thread::sleep(Duration::from_secs(1));
    let pass = held.pass(&b);
    assert!(
        !held.test_box.trunk_change_ids(&held.remote).contains(&doc),
        "the document passed a head under a long check: {pass}"
    );

    // The head lands on its first check; then the document lands.
    held.go();
    let ran = finished(runner_a);
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&slow),
        "the head did not land: {ran}"
    );
    assert!(
        !ran.to_string().contains("main moved"),
        "the head was checked again: {ran}"
    );
    let pass = held.pass(&b);
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&doc),
        "the document never landed: {pass}"
    );
}

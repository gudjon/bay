//! G554 (a fleet finding, 2026-10-29: a change that needed no check ran from its own place while
//! the head checked, and planned a batch of ten from the line behind it; the two batches claimed
//! the same members, one landed, and the other gave every member back). A head that runs ahead of
//! its turn lands alone, so the changes behind it wait for the queue's head.
//!
//! G403 runs a change that needs no check from its own place. With `batch` set it planned a batch
//! like any head, so a change that needs a check landed past the head, under the out-of-turn
//! head's batch.
//!
//! Guard to mutate, in `src/publish.rs`: the G554 return in `batch_plan` (the change behind the
//! document lands in the document's batch).

#![cfg(unix)]

use crate::support;

use std::time::Duration;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};
use support::{check_files, passing_result};

#[test]
fn a_document_ahead_of_its_turn_takes_no_change_from_the_line() {
    // One pre-merge check, required only for src/, whose unsigned evidence a batch reuses.
    let policy = check_files("x", &passing_result(1), 1, 120)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| {
            text.replacen("timeout_s", "when_paths = [\"src/**\"]\ntimeout_s", 1)
                + "\n[policy]\nreuse_unsigned = true\n"
        })
        .unwrap();
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 4\nbatch = { max = 4 }\n",
        policy: Some(&policy),
        seed: &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
        hold_first: true,
        fails: "false",
    });
    let slow = held.queue("slow", "src");
    let note = held.queue_with("note", &["docs"], &[("docs/note.md", "a note\n")]);
    let after = held.queue("after", "src");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let runner_a = held.spawn(&a);
    held.wait_held();
    std::thread::sleep(Duration::from_secs(1));
    let pass = held.pass(&b);
    let merged = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        merged.contains(&note),
        "the document waited behind the head: {pass}"
    );
    assert!(
        !merged.contains(&after),
        "the change behind the document landed in its batch, past the head: {pass}"
    );
    held.go();
    let _ = finished(runner_a);
    let pass = held.pass(&b);
    assert!(
        held.test_box
            .trunk_change_ids(&held.remote)
            .contains(&after),
        "the change behind the document never landed: {pass}"
    );
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&slow),
        "the head never landed"
    );
}

//! G403 (owner, 2026-10-26: "basic pushes, like research documents, should be immediate"):
//! nothing to check, nothing to wait. A change that no pre-merge check covers runs as a head
//! from its own place and merges in the next push, while the head still runs; the host takes
//! that publication. A change a check does cover waits in order.
//!
//! Guard to mutate, in `src/publish.rs`: `needs_no_check` returning `false`.

#![cfg(unix)]

use crate::support;

use std::time::Duration;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};
use support::{check_files, passing_result};

#[test]
fn a_document_merges_while_the_head_still_runs() {
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
    let note = held.queue_with("note", &["docs"], &[("docs/note.md", "a note\n")]);
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
        !merged.contains(&slow),
        "the head was still running: {merged:?}"
    );
    held.go();
    let _ = finished(runner_a);
}

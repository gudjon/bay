//! G901. A place is released only for the operation it names.
//!
//! A runner releases the place of the operation it ran when the run ends any way but a landing.
//! Measured 2026-11-25, a newer revision of a change took its place while a runner still ran the
//! old operation; 43 minutes later that runner released the place for the old operation, and the
//! release refused the atomic push of a batch of eleven that held the place. `queue::release` now
//! leaves alone a place this store reads as another operation's, and leases the place's entry with
//! the release, so a place another operation took since the read is left alone too.
//!
//! The guards to mutate, in `queue::release` (`src/queue.rs`): drop the entry lease and the
//! first case releases the newer operation's place; drop the read of the entry's operation and
//! the second does.

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::queued::{Check, Held, Trust, host_under};

fn held() -> Held {
    host_under(
        &Check {
            config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
            policy: None,
            seed: &[("src/a.rs", "fn a() {}\n")],
            hold_first: false,
            fails: "false",
        },
        Trust::Unsigned,
    )
}

/// The remote's release of place `seq`, empty when it has none.
fn release_on_remote(held: &Held, seq: u64) -> String {
    held.test_box.git_bare(
        &held.remote,
        &["for-each-ref", &format!("refs/bay/queue-done/{seq}")],
    )
}

#[test]
fn a_place_another_operation_took_after_the_read_is_not_released() {
    let held = held();
    let one = held.queue("one", "src");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    let read = bay::queue::Queue::read(&opened).unwrap();
    let entry = read.of(&format!("{one}.1")).cloned().unwrap();
    // A newer revision takes the place on the remote; this store still reads the old entry.
    let replaced = bay::queue::replace(&opened, None, &entry, &format!("{one}.2")).unwrap();
    assert!(replaced, "the newer revision did not take the place");
    // The runner that ran the old operation ends it and releases its place.
    bay::queue::release(&opened, None, entry.seq, &entry.op_id, "failed").unwrap();
    let released = release_on_remote(&held, entry.seq);
    assert!(
        released.trim().is_empty(),
        "the newer operation's place was released for the old one: {released}"
    );
}

#[test]
fn a_place_this_store_reads_as_another_operations_is_not_released() {
    let held = held();
    let one = held.queue("one", "src");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    let read = bay::queue::Queue::read(&opened).unwrap();
    let entry = read.of(&format!("{one}.1")).cloned().unwrap();
    assert!(bay::queue::replace(&opened, None, &entry, &format!("{one}.2")).unwrap());
    // This store reads the place again and holds the newer operation's entry.
    let again = bay::queue::Queue::read(&opened).unwrap();
    assert!(
        again.of(&format!("{one}.2")).is_some(),
        "the store did not read the new entry"
    );
    bay::queue::release(&opened, None, entry.seq, &entry.op_id, "failed").unwrap();
    let released = release_on_remote(&held, entry.seq);
    assert!(
        released.trim().is_empty(),
        "the newer operation's place was released for the old one: {released}"
    );
    // The place's own operation still releases it.
    bay::queue::release(&opened, None, entry.seq, &format!("{one}.2"), "failed").unwrap();
    assert!(
        !release_on_remote(&held, entry.seq).trim().is_empty(),
        "the place's own operation could not release it"
    );
}

//! G424. A newer revision never takes a queue place that was already released.
//!
//! A runner gives an ended operation's place to the revision that superseded it, from the
//! queue it read at its pass's start. Measured 2026-10-27, a revision was written onto a place
//! released as published a minute before, its change never read as ended, and every runner
//! retook it each pass for ten hours. The replacement's lease guards only the entry's ref, so
//! `queue::replace` reads the place's release on the remote first and leaves a released place
//! alone; the newer operation joins at its own turn.
//!
//! The guard to mutate: the release read in `queue::replace` (`src/queue.rs`); drop it and the
//! released place takes the newer revision.

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::queued::{Check, Trust, host_under};

#[test]
fn a_place_released_after_the_read_is_not_replaced() {
    let held = host_under(
        &Check {
            config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
            policy: None,
            seed: &[("src/a.rs", "fn a() {}\n")],
            hold_first: false,
            fails: "false",
        },
        Trust::Unsigned,
    );
    let one = held.queue("one", "src");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    // The queue as a pass read it at its start.
    let read = bay::queue::Queue::read(&opened).unwrap();
    let entry = read.of(&format!("{one}.1")).cloned().unwrap();
    // The place is released after that read, as a landing releases it.
    bay::queue::release(&opened, None, entry.seq, &entry.op_id, "published").unwrap();
    let replaced = bay::queue::replace(&opened, None, &entry, &format!("{one}.2")).unwrap();
    assert!(!replaced, "a released place took the newer revision");
    let place = held.test_box.git_bare(
        &held.remote,
        &["show", &format!("refs/bay/queue/{}:entry.json", entry.seq)],
    );
    assert!(!place.contains(&format!("{one}.2")), "{place}");
}

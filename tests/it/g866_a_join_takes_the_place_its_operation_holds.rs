//! G866 (a fleet measurement, 2026-11-23: in one hour 27 of 60 places in the queue were
//! released as duplicates, each reserved up to 37 s after its operation's first place). A
//! runner's visit decides to join on the queue its pass read, up to five minutes old (G779), so
//! it joined an operation another runner had placed since: the join pushed a second place and
//! then released it. Now a join whose own read shows a live place of its operation takes that
//! place and pushes nothing.
//!
//! Guard to mutate: in `src/queue.rs` `join`, drop the return of the operation's live place
//! (a second place is pushed and released as a duplicate).

// The host's check runs shell scripts, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use support::queued::{Check, host};
use support::{EXECUTOR, PRODUCER};

#[test]
fn a_join_of_an_operation_the_queue_holds_takes_its_place_and_pushes_nothing() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let refs = |prefix: &str| {
        held.test_box
            .git_bare(
                &held.remote,
                &["for-each-ref", "--format=%(refname)", prefix],
            )
            .trim()
            .to_owned()
    };

    // GIVEN one change that holds a place
    let id = held.queue("one", "src");
    let places = refs("refs/bay/queue/");
    assert_eq!(places.lines().count(), 1, "{places}");

    // WHEN a runner that read no place for it joins it
    let runner = held.executor("exec", PRODUCER);
    let opened = bay::repo::Repo::open(&runner).unwrap();
    let signer = bay::signing::Signer {
        key: held.keys.executor.clone(),
        name: EXECUTOR.to_owned(),
    };
    let seq = bay::queue::join(&opened, Some(&signer), &format!("{id}.1"), &id).unwrap();

    // THEN it takes the place the queue holds, and no second place is pushed or released
    assert_eq!(format!("refs/bay/queue/{seq}"), places);
    assert_eq!(refs("refs/bay/queue/"), places);
    assert_eq!(refs("refs/bay/queue-done/"), "");
}

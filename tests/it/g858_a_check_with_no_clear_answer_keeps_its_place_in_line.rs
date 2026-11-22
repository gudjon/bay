//! G858 (a fleet finding, 2026-11-22: a change whose check gave no clear answer at the head was
//! re-armed a minute later, once main moved, and joined the line at the back, behind 26 places
//! that queued meanwhile, so it waited 54 minutes). A head released because its check gave no
//! clear answer (`CHECK_UNKNOWN`) takes its place back when a pass re-arms it: it is the head
//! again, ahead of the changes that queued behind it.
//!
//! The check gives no result (the adapter exits before it prints one) for a tree with
//! `unknown/` while `conf/state` says `away`.
//!
//! Guards to mutate, in `src/publish.rs` `retry_blocked`: drop the `reopen_place` call (the
//! change joins at the back and waits behind the held run); in `src/queue.rs` `reopen`: drop the
//! release's refspec from the push (the place stays released, and the change joins at the back).

// Each case runs shell scripts (the adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};

/// The shell condition the host's check tests: no result for a tree with `unknown/` while
/// `conf/state` says `away`, and never a failure.
const UNKNOWN_WHILE_AWAY: &str =
    "{ [ -d unknown ] && [ \"$(cat conf/state)\" = away ] && exit 0; false; }";

#[test]
fn a_change_whose_check_gave_no_answer_takes_its_place_back_when_it_is_asked_again() {
    // GIVEN three queued changes: `stuck`, which the check gives no answer while main says
    // away, `state`, which makes main say otherwise, and `next`, whose run waits.
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n"), ("conf/state", "away\n")],
        hold_first: false,
        fails: UNKNOWN_WHILE_AWAY,
    });
    let stuck = held.queue("stuck", "unknown");
    let state = held.queue_with("state", &["conf"], &[("conf/state", "pass\n")]);
    let next = held.queue("next", "src");
    held.hold_run(3);
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);

    // WHEN A's pass leaves `stuck` blocked, lands `state`, and holds `next`'s run at the head.
    let runner_a = held.spawn(&a);
    held.wait_held_run(3);
    let landed = held.test_box.trunk_change_ids(&held.remote);
    assert!(landed.contains(&state), "main never moved: {landed:?}");
    assert!(!landed.contains(&stuck), "{landed:?}");
    let released = held
        .test_box
        .git_bare(&held.remote, &["show", "refs/bay/queue-done/1:release.json"]);
    assert!(
        released.contains(&format!("{stuck}.1")) && released.contains("check_unknown"),
        "stuck's place was not released as no answer: {released}"
    );

    // THEN B's pass asks `stuck` again on the new main: it holds its own place again, the head,
    // and lands while `next` still waits at the place behind it.
    let pass = held.pass(&b);
    let landed = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        landed.contains(&stuck),
        "stuck did not take its place back: {landed:?} after {pass}"
    );
    assert!(!landed.contains(&next), "{landed:?}");
    let places = held.test_box.git_bare(
        &held.remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/queue/"],
    );
    assert!(
        !places.contains("refs/bay/queue/4"),
        "stuck joined at the back: {places}"
    );
    held.go_run(3);
    let _ = finished(runner_a);
}

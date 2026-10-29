//! G451 (T10, the owner's probe on 0.5.39, again on 0.5.42). A first read lists a talk that
//! closed already.
//!
//! `bay status --since` lists a talk's close only when its cursor held the talk open, so a read
//! with no cursor never showed a talk that closed before it: a sender whose talk was answered
//! between its send and its first read never learned of the answer there. Now a read with no
//! cursor lists each talk the actor sent or must answer that closed within the window the
//! sender's replies are told in (`replies::RECENT_S`, a week), once, with its answer; the cursor
//! it returns holds open talks only, so a read from it lists that close no more. A talk that
//! closed before the window is not listed.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/talk/standing.rs` `followed`: the `None` (no cursor) arm keeps no closed talk.
//! - `src/talk/standing.rs` `followed`: the window check dropped (a talk closed before it is
//!   listed).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::talk::{Approvals, approvals_talk};

fn closes(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| event["kind"] == "talk_closed")
        .collect()
}

/// A discovery talk from `agent` to `agent-b`, answered `blue` before `agent` reads anything.
fn answered_before_any_read(case: &Approvals) -> String {
    let id = case.ask(
        "agent",
        "agent-b",
        "Pick a colour",
        &["--kind", "discovery", "--decision", "colour"],
        &[],
    );
    let (code, answered) = case.bay("agent-b", &["talk", "--answer", &id, "--value", "blue"]);
    assert_eq!(code, 0, "{answered}");
    id
}

#[test]
fn a_read_with_no_cursor_lists_a_talk_that_closed_before_it_once() {
    let case = approvals_talk();
    let id = answered_before_any_read(&case);

    let (events, cursor) = case.since("agent", None, &id, &[]);
    let closed = closes(&events);
    assert_eq!(closed.len(), 1, "{events:?}");
    assert_eq!(closed[0]["state"], "answered", "{events:?}");
    assert_eq!(closed[0]["answer"], "blue", "{events:?}");
    assert!(
        events.iter().all(|event| event["kind"] != "talk_arrived"),
        "{events:?}"
    );
    // The cursor holds open talks only: from it, the close is not listed again.
    let (again, _) = case.since("agent", Some(&cursor), &id, &[]);
    assert!(again.is_empty(), "{again:?}");

    // The recipient's own first read lists it too: it must answer it.
    let (theirs, _) = case.since("agent-b", None, &id, &[]);
    assert_eq!(closes(&theirs).len(), 1, "{theirs:?}");
}

#[test]
fn a_talk_that_closed_before_the_window_is_not_listed() {
    let case = approvals_talk();
    let id = answered_before_any_read(&case);
    // The reader's clock is past the window, as if the read came eight days later.
    let skew = (bay::talk::replies::RECENT_S + 3600).to_string();
    let (events, _) = case.since(
        "agent",
        None,
        &id,
        &[("BAY_TEST_TALK_CLOCK_SKEW_S", skew.as_str())],
    );
    assert!(closes(&events).is_empty(), "{events:?}");
}

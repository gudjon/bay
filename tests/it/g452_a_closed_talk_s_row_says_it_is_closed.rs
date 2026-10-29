//! G452 (T11, the owner's probe on 0.5.39, again on 0.5.42). A closed talk's row says it is
//! closed, and an answer or a backbrief by its recipient is its arrival.
//!
//! The row of a closed talk was read by the open-talk rules: after talk, ack and answer,
//! `talk_closed` said `standing: arrived` and `owes: <recipient>`; after an answer with no ack,
//! `not_on_recipient`, `ack_age_s: null` and `reply_age_s: null`. Now a closed talk's row says
//! `standing: "closed"` and owes nothing (`owes: null`). An answer or a backbrief written by `who`
//! before any ack is the talk's arrival, so `ack_age_s` is its age; an answer by `decides` is not
//! (it says nothing of `who`). `reply_age_s` counts an answer as well as a backbrief, so an open
//! talk with a backbrief and no ack is `reply_owed`, never `not_on_recipient`.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/talk/standing.rs` `read_row`: the `talk.closed()` arm to the open-talk standing.
//! - `src/talk/standing.rs` `read_row`: `owes` kept for a closed talk.
//! - `src/talk/standing.rs` `arrival`: an answer or a backbrief no longer counts (the ack only).
//! - `src/talk/standing.rs` `arrival`: the `who` check on an answer dropped (the answer by
//!   `decides` counts as an arrival).
//! - `src/talk/standing.rs` `read_row`: the latest reply read from backbriefs only.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::{Value, json};
use support::talk::{approvals_talk, talk_ref};

fn closed(events: &[Value]) -> &Value {
    events
        .iter()
        .find(|event| event["kind"] == "talk_closed")
        .unwrap_or_else(|| panic!("no talk_closed: {events:?}"))
}

fn discovery(decision: &str) -> [&str; 4] {
    ["--kind", "discovery", "--decision", decision]
}

#[test]
fn a_talk_acked_then_answered_reads_closed_and_owes_nothing() {
    let case = approvals_talk();
    let id = case.ask(
        "agent",
        "agent-b",
        "Pick a colour",
        &discovery("colour"),
        &[],
    );
    let (_, before) = case.since("agent", None, &id, &[]);
    let (code, status) = case.bay("agent-b", &["status"]);
    assert_eq!(code, 0, "{status}");
    let (code, answered) = case.bay("agent-b", &["talk", "--answer", &id, "--value", "blue"]);
    assert_eq!(code, 0, "{answered}");

    let (events, _) = case.since("agent", Some(&before), &id, &[]);
    assert!(!events.is_empty(), "{events:?}");
    for event in &events {
        assert_eq!(event["standing"], "closed", "{event}");
        assert_eq!(event["owes"], Value::Null, "{event}");
        assert!(event["ack_age_s"].is_u64(), "{event}");
        assert!(event["reply_age_s"].is_u64(), "{event}");
    }
    assert_eq!(closed(&events)["state"], "answered", "{events:?}");
}

#[test]
fn an_answer_with_no_ack_is_the_talk_s_arrival() {
    let case = approvals_talk();
    let id = case.ask("agent", "agent-b", "Pick a size", &discovery("size"), &[]);
    let (_, before) = case.since("agent", None, &id, &[]);
    // `agent-b` answers before any read of its own acks the talk.
    let name = talk_ref("agent-b", &id);
    case.test_box.git_ok(
        &case.b,
        &["fetch", "-q", "origin", &format!("+{name}:{name}")],
    );
    let (code, answered) = case.bay("agent-b", &["talk", "--answer", &id, "--value", "large"]);
    assert_eq!(code, 0, "{answered}");

    let (events, _) = case.since("agent", Some(&before), &id, &[]);
    assert!(!events.is_empty(), "{events:?}");
    for event in &events {
        assert_eq!(event["standing"], "closed", "{event}");
        assert!(
            event["ack_age_s"].is_u64(),
            "the answer is the arrival: {event}"
        );
        assert!(event["reply_age_s"].is_u64(), "{event}");
    }
}

#[test]
fn an_answer_by_decides_is_no_arrival() {
    let case = approvals_talk();
    // The sender decides: its own answer closes the talk and says nothing of `agent-b`.
    let id = case.ask(
        "agent",
        "agent-b",
        "Pick a font",
        &discovery("font"),
        &["--decides", "agent"],
    );
    let (_, before) = case.since("agent", None, &id, &[]);
    let (code, answered) = case.bay("agent", &["talk", "--answer", &id, "--value", "serif"]);
    assert_eq!(code, 0, "{answered}");

    let (events, _) = case.since("agent", Some(&before), &id, &[]);
    let event = closed(&events);
    assert_eq!(event["standing"], "closed", "{event}");
    assert_eq!(event["ack_age_s"], Value::Null, "{event}");
    assert!(event["reply_age_s"].is_u64(), "{event}");
}

#[test]
fn an_open_talk_with_a_backbrief_and_no_ack_owes_its_reply() {
    let case = approvals_talk();
    let id = case.ask(
        "agent",
        "agent-b",
        "Ship the parser change",
        &["--kind", "delivery", "--change", "abcd2345"],
        &[],
    );
    case.entry(
        "agent-b",
        "agent-b",
        &id,
        "backbrief",
        &json!({"status": "ready", "evidence": "abcd2345"}),
    );
    let (code, status) = case.bay("agent", &["status"]);
    assert_eq!(code, 0, "{status}");
    let row = status["detail"]["talks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["talk"] == id.as_str())
        .unwrap_or_else(|| panic!("no row for {id}: {status}"))
        .clone();
    assert_eq!(row["standing"], "reply_owed", "{row}");
    assert!(row["ack_age_s"].is_u64(), "{row}");
    assert!(row["reply_age_s"].is_u64(), "{row}");
}

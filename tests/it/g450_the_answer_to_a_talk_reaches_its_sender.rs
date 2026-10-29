//! G450 (T9, the owner's probe on 0.5.39, again on 0.5.42). The answer to a talk reaches its
//! sender.
//!
//! `talk_answered` and `talk_closed` in `bay status --since` carried the talk's row only, so the
//! sender read that its talk was answered and not what the answer was: the value was only in
//! `entries/NNNN-answer.json` on the talk's chain. Now each talk event carries the reply: `answer`,
//! the latest answer's value as recorded, and `reply`, the latest backbrief's `status` and
//! `evidence`. This JSON is data the actor asked for; a notice still never prints a talk's text
//! (ruling R13, G447). With no runner anywhere, the recipient's first read lists the talk's
//! arrival and the sender's read lists its close (F5).
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/talk/standing.rs` `followed`: `answer` read as `Value::Null` (no event carries it).
//! - `src/talk/standing.rs` `followed`: the answer read by `replies::shown_value` instead of
//!   `replies::recorded_value` (a value that is not a plain token is dropped).
//! - `src/talk/standing.rs` `followed`: `reply` read as `Value::Null`.
//! - `src/watch.rs` `talk_events`: the two event fields left out.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::{Value, json};
use support::talk::approvals_talk;

fn of<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["kind"] == kind)
        .collect()
}

#[test]
fn the_answer_value_is_in_the_events_the_sender_reads() {
    let case = approvals_talk();
    let id = case.ask(
        "agent",
        "agent-b",
        "Pick the border colour",
        &["--kind", "discovery", "--decision", "border"],
        &[],
    );
    // The sender's cursor holds the open talk.
    let (_, before) = case.since("agent", None, &id, &[]);

    // F5: with no runner, the recipient's first read accepts the talk and lists its arrival.
    let (arrived, _) = case.since("agent-b", None, &id, &[]);
    let arrival = of(&arrived, "talk_arrived");
    assert_eq!(arrival.len(), 1, "{arrived:?}");
    assert_eq!(arrival[0]["from"], "agent", "{arrived:?}");
    assert_eq!(arrival[0]["answer"], Value::Null, "{arrived:?}");

    // The answer is not a plain token: a notice would leave it out, the events carry it whole.
    let value = "blue, with a grey edge";
    let (code, answered) = case.bay("agent-b", &["talk", "--answer", &id, "--value", value]);
    assert_eq!(code, 0, "{answered}");

    let (events, after) = case.since("agent", Some(&before), &id, &[]);
    let answered = of(&events, "talk_answered");
    let closed = of(&events, "talk_closed");
    assert_eq!(answered.len(), 1, "{events:?}");
    assert_eq!(closed.len(), 1, "{events:?}");
    for event in answered.iter().chain(&closed) {
        assert_eq!(event["answer"], value, "{event}");
        assert_eq!(event["reply"], Value::Null, "{event}");
    }
    assert_eq!(closed[0]["state"], "answered", "{events:?}");
    let (again, _) = case.since("agent", Some(&after), &id, &[]);
    assert!(again.is_empty(), "{again:?}");
}

#[test]
fn a_backbrief_s_status_and_evidence_are_in_the_events_the_sender_reads() {
    let case = approvals_talk();
    let id = case.ask(
        "agent",
        "agent-b",
        "Ship the parser change",
        &["--kind", "delivery", "--change", "abcd2345"],
        &[],
    );
    let (code, status) = case.bay("agent-b", &["status"]);
    assert_eq!(code, 0, "{status}");
    let (_, before) = case.since("agent", None, &id, &[]);

    case.entry(
        "agent-b",
        "agent-b",
        &id,
        "backbrief",
        &json!({"status": "blocked", "evidence": "abcd2345", "outcome": "The lexer has to land first"}),
    );
    let (events, _) = case.since("agent", Some(&before), &id, &[]);
    let answered = of(&events, "talk_answered");
    assert_eq!(answered.len(), 1, "{events:?}");
    assert_eq!(
        answered[0]["reply"],
        json!({"status": "blocked", "evidence": "abcd2345"}),
        "{events:?}"
    );
    assert_eq!(answered[0]["answer"], Value::Null, "{events:?}");
}

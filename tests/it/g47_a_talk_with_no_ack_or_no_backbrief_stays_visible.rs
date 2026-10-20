//! G47: a talk with no ack or no backbrief stays visible (`DESIGN` § 11c, "What status shows").
//!
//! `bay status` lists each open talk the actor sent or must answer: who owes the reply, the age
//! from `sent`, the ack's age, and its standing. Before the recipient accepts it the sender sees
//! `not_on_recipient`; once accepted, `arrived`; past its `follow_up` with no backbrief, `stale`,
//! and the row stays. (The 24-hour lost arrival is the unit test in `talk::standing`.)

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::talk::{from_now, hosted_talk};

fn talk_row(status: &Value, id: &str) -> Value {
    status["detail"]["talks"]
        .as_array()
        .unwrap_or_else(|| panic!("no talks in {status}"))
        .iter()
        .find(|row| row["talk"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("talk {id} not listed: {status}"))
}

#[test]
fn status_lists_a_talk_until_it_closes_and_names_who_owes_the_reply() {
    let case = hosted_talk();
    let follow_up = from_now(6);
    let (code, sent) = case.talk(
        "agent",
        &[
            "agent-b",
            "Review the parser change",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();

    let (_, status) = case.status("agent");
    let row = talk_row(&status, &id);
    assert_eq!(row["standing"], "not_on_recipient", "{row}");
    assert_eq!(row["owes"], "agent-b", "{row}");
    assert_eq!(row["ack_age_s"], Value::Null, "{row}");
    assert!(row["age_s"].is_u64(), "{row}");

    // The recipient's status accepts it: arrived, acked, the reply still owed by agent-b.
    let (_, status) = case.status("agent-b");
    let row = talk_row(&status, &id);
    assert_eq!(row["standing"], "arrived", "{row}");
    assert!(row["ack_age_s"].is_u64(), "{row}");

    // Past follow_up with no backbrief: stale, and still listed.
    std::thread::sleep(std::time::Duration::from_secs(7));
    let (_, status) = case.status("agent-b");
    let row = talk_row(&status, &id);
    assert_eq!(row["standing"], "stale", "{row}");
    assert_eq!(row["owes"], "agent-b", "{row}");
}

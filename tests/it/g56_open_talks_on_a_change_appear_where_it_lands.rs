//! G56: open talks on a change appear where it lands (`DESIGN` § 11c; ruling T9).
//!
//! `bay land` and `bay undo` fetch the acting actor's talk refs before they act, and their result
//! lists under `detail.open_talks` every talk to that actor, unread or open, whose target is the
//! change. A talk about another change is not listed. The fetch writes no ack, so an unread talk
//! stays unread, and the verbs never block on a talk.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::Value;
use support::result_json;
use support::talk::{TalkCase, from_now, hosted_talk_landing, talk_ref};

/// Starts a change on `src/` as `agent` and writes `file` in its view. Returns (id, view).
fn open(case: &TalkCase, intent: &str, file: &str) -> (String, PathBuf) {
    let (code, started) = result_json(
        case.bay_as("agent")
            .args(["start", intent, "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{started}");
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), "fn changed() {}\n").unwrap();
    (id, view)
}

/// A delivery talk from `agent-b` to `agent` about `change`. Returns its id.
fn ask_about(case: &TalkCase, change: &str, what: &str) -> String {
    let follow_up = from_now(3600);
    let (code, sent) = case.talk(
        "agent-b",
        &[
            "agent",
            what,
            "--kind",
            "delivery",
            "--change",
            change,
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{sent}");
    sent["detail"]["talk"].as_str().unwrap().to_owned()
}

/// The talk ids a result lists under `detail.open_talks`.
fn open_talks(result: &Value) -> Vec<String> {
    result["detail"]["open_talks"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| row["talk"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn land_and_undo_name_the_unread_talks_on_their_change_and_no_other() {
    let case = hosted_talk_landing();
    let (c, view) = open(&case, "CARD-c", "src/c.rs");
    // Another change: one this actor has not opened here.
    let d = "abcd2345";
    let on_c = ask_about(&case, &c, "Hold c until the schema lands");
    let on_d = ask_about(&case, d, "Rename the d module first");
    // The talks are on the host only: `agent`'s clone has not fetched them.
    assert_eq!(case.local_tip(&case.a, &talk_ref("agent", &on_c)), None);

    let (code, landed) = result_json(
        case.test_box
            .bay(&view)
            .env("BAY_ACTOR", "agent")
            .env("BAY_SESSION", "s-agent")
            .args(["land", "--wait"]),
    );
    assert_eq!(code, 0, "a talk never blocks a landing: {landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert_eq!(open_talks(&landed), vec![on_c.clone()], "{landed}");
    let row = &landed["detail"]["open_talks"][0];
    assert_eq!(row["from"], "agent-b", "{row}");
    assert_eq!(row["kind"], "delivery", "{row}");
    assert_eq!(row["read"], false, "the fetch is no accept: {row}");
    let tip = case.local_tip(&case.a, &talk_ref("agent", &on_c)).unwrap();
    assert!(
        !case
            .files(Some(case.a.as_path()), &tip)
            .iter()
            .any(|path| path.starts_with("entries/")),
        "land wrote no ack"
    );
    assert!(!open_talks(&landed).contains(&on_d), "{landed}");

    let (code, undone) = result_json(case.bay_as("agent").args(["undo", &c, "--wait"]));
    assert_eq!(code, 0, "{undone}");
    assert_eq!(open_talks(&undone), vec![on_c], "{undone}");
}

//! G464 (bay 0.6.0, T3; H4 of the owner's probe). Under `[policy] signing = "approvals"` any
//! actor name may be a talk's `who` or `decides` (G445), so a typo sends a talk nobody reads. The
//! send still goes, and its result names each name no record here names, in `detail.unseen`
//! with a sentence in `detail.warning`, so the typo is seen at once. A name is seen when a talk
//! to it is here or on the remote, when a change record this clone holds names it as its actor,
//! or when it is the sender's own. Under `all` the registry already refuses a name it does not
//! enrol, so the result names none.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/talk/send.rs` `unseen`: return no names (the typo goes with no warning).
//! - `src/talk/send.rs` `unseen`: drop the talk-ref source (a name already talked to is
//!   warned again).
//! - `src/talk/send.rs` `unseen`: drop the change-record source (an actor with a change is
//!   warned), or the sender's own name (a note to oneself is warned).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::{Value, json};
use support::plain_talk::Plain;
use support::talk::from_now;

fn unseen(result: &Value) -> Value {
    result["detail"]["unseen"].clone()
}

#[test]
fn a_name_no_record_names_is_sent_and_warned() {
    let case = Plain::approvals();
    let follow_up = from_now(3600);
    let (code, typo) = case.talk(
        "agent",
        &[
            "agent-bb",
            "Pick the wire format",
            "--kind",
            "discovery",
            "--decision",
            "wire",
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{typo}");
    assert_eq!(typo["state"], "sent", "{typo}");
    assert_eq!(unseen(&typo), json!(["agent-bb"]), "{typo}");
    let warning = typo["detail"]["warning"].as_str().unwrap_or_default();
    assert!(warning.contains("\"agent-bb\""), "{typo}");
    let (code, dry) = case.talk("agent", &["agent-cc", "Pick", "--dry-run"]);
    assert_eq!(code, 0, "{dry}");
    assert_eq!(unseen(&dry), json!(["agent-cc"]), "{dry}");

    // A talk to it is a record that names it.
    let (code, again) = case.talk("agent", &["agent-bb", "Pick the log format"]);
    assert_eq!(code, 0, "{again}");
    assert!(unseen(&again).is_null(), "{again}");

    // So is a change record of its, once this clone holds it; and the sender is its own name.
    case.bay("agent-b", &["start", "CARD-1", "--paths", "src/"]);
    case.test_box.git_ok(
        &case.a,
        &[
            "fetch",
            "-q",
            "origin",
            "refs/bay/changes/*:refs/bay/changes/*",
        ],
    );
    let (code, known) = case.talk("agent", &["agent-b", "Review the parser"]);
    assert_eq!(code, 0, "{known}");
    assert!(unseen(&known).is_null(), "{known}");
    let (code, own) = case.talk("agent", &["agent", "A note to myself"]);
    assert_eq!(code, 0, "{own}");
    assert!(unseen(&own).is_null(), "{own}");

    // `decides` is judged the same way.
    let (code, decides) = case.talk(
        "agent",
        &["agent-b", "Pick the field name", "--decides", "agent-x"],
    );
    assert_eq!(code, 0, "{decides}");
    assert_eq!(unseen(&decides), json!(["agent-x"]), "{decides}");
}

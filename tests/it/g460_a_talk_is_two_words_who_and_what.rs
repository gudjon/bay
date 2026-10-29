//! G460 (bay 0.6.0, T1; the owner's probe I5, 2026-10-28: "make sure we have bay talk at its full
//! simplicity"). `bay talk <who> "<what>"`, two words and no flag, sends a talk: a discovery talk
//! whose decision is its `what`, owed a reply the policy's default time after it is sent
//! (`[talk] follow_up_s`, one hour when the policy names none). A target given alone names its
//! kind (`--change` or `--artifact` a delivery talk, `--check` a reliability talk), and every
//! flag given stays as given. A `follow_up_s` of 0 owes the reply at once, which no talk can:
//! the policy is `POLICY_INVALID`.
//!
//! Before 0.6.0 the two-word send was `TALK_REFUSED`: "a talk needs `kind`".
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/talk/send.rs` `with_defaults`: the kind default `"discovery"` to `None` (the two-word
//!   send is refused again), or drop the decision default (a discovery talk with no target).
//! - `src/talk/send.rs` `with_defaults`: the follow-up default to a fixed 3600 (the policy's
//!   600 is not read).
//! - `src/talk.rs` `kind_of_target`: always `None` (a `--change` alone is a discovery talk,
//!   refused for its target).
//! - `src/policy.rs` `load_at`: drop the `follow_up_s = 0` refusal.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::plain_talk::{APPROVALS, Plain, TALK_ON};
use support::result_json;
use support::talk::{from_now, talk_ref};

fn unix(body: &Value, field: &str) -> u64 {
    bay::clock::unix_of(body[field].as_str().unwrap()).unwrap()
}

/// The body of the talk `sent` names, on the remote.
fn sent_body(case: &Plain, sent: &Value) -> Value {
    let id = sent["detail"]["talk"].as_str().unwrap();
    case.body(&talk_ref("agent-b", id))
}

#[test]
fn two_words_send_a_discovery_talk_owed_by_the_default_reply_time() {
    let case = Plain::approvals();
    let what = "Pick the wire format";
    let (code, dry) = case.talk("agent", &["agent-b", what, "--dry-run"]);
    assert_eq!(code, 0, "{dry}");
    assert_eq!(dry["detail"]["dry_run"], true, "{dry}");
    assert_eq!(
        case.remote_talks(),
        Vec::<String>::new(),
        "a dry run sent a talk"
    );

    let (code, sent) = case.talk("agent", &["agent-b", what]);
    assert_eq!(code, 0, "{sent}");
    assert_eq!(sent["state"], "sent", "{sent}");
    assert_eq!(sent["detail"]["pushed"], true, "{sent}");
    let body = sent_body(&case, &sent);
    assert_eq!(body["kind"], "discovery", "{body}");
    assert_eq!(body["what"], what, "{body}");
    assert_eq!(
        body["decision"], what,
        "the decision is the talk's own what: {body}"
    );
    assert_eq!(
        unix(&body, "follow_up") - unix(&body, "sent"),
        3600,
        "one hour when the policy names no follow_up_s: {body}"
    );
}

#[test]
fn the_policy_names_the_default_reply_time() {
    let case = Plain::new(&format!("{TALK_ON}follow_up_s = 600\n{APPROVALS}"));
    let (code, sent) = case.talk("agent", &["agent-b", "Pick the log format"]);
    assert_eq!(code, 0, "{sent}");
    let body = sent_body(&case, &sent);
    assert_eq!(
        unix(&body, "follow_up") - unix(&body, "sent"),
        600,
        "{body}"
    );
}

#[test]
fn a_target_given_alone_names_its_kind_and_every_flag_stays_as_given() {
    let case = Plain::approvals();
    let (code, sent) = case.talk(
        "agent",
        &["agent-b", "Ship the parser", "--change", "abcd2345"],
    );
    assert_eq!(code, 0, "{sent}");
    let body = sent_body(&case, &sent);
    assert_eq!(body["kind"], "delivery", "{body}");
    assert_eq!(body["change"], "abcd2345", "{body}");
    assert!(body.get("decision").is_none(), "{body}");

    let (code, sent) = case.talk("agent", &["agent-b", "Run the lint", "--check", "lint"]);
    assert_eq!(code, 0, "{sent}");
    assert_eq!(sent_body(&case, &sent)["kind"], "reliability");

    let follow_up = from_now(7200);
    let (code, sent) = case.talk(
        "agent",
        &[
            "agent-b",
            "Pick a colour",
            "--kind",
            "discovery",
            "--decision",
            "colour",
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{sent}");
    let body = sent_body(&case, &sent);
    assert_eq!(body["decision"], "colour", "{body}");
    assert_eq!(body["follow_up"], follow_up.as_str(), "{body}");

    // A kind that does not own the target given is still refused, by the kind's own rule.
    let (code, refused) = case.talk(
        "agent",
        &[
            "agent-b",
            "Pick",
            "--kind",
            "reliability",
            "--change",
            "abcd2345",
        ],
    );
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "TALK_REFUSED", "{refused}");
}

#[test]
fn a_reply_time_of_zero_is_policy_invalid() {
    let case = Plain::new(&format!("{TALK_ON}follow_up_s = 0\n{APPROVALS}"));
    let (_, view) = case.test_box.start_and_edit(&case.a);
    let (code, refused) = result_json(case.test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
    assert!(
        refused["detail"].to_string().contains("follow_up_s"),
        "{refused}"
    );
}

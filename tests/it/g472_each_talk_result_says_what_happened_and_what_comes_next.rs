//! G472 (T12, the owner's probe of bay talk, 2026-10-28). Each talk result says what happened
//! and what comes next.
//!
//! Every talk success fell through to the last sentence an `ok` has, "Done; nothing else is
//! needed.": a dry run that sent nothing, a send that leaves a reply owed, a retry, an answer and
//! a repeat answer. A dry run and an open ask both read as finished. Now each says its own: a dry
//! run that nothing was sent; a send who owes the reply and by when; a retry that it was sent
//! before; an answer that the sender is told; a backbrief its status word, and a repeat of
//! either that nothing new was sent. The talk-off refusal, a round of talks and a talk kept here
//! for want of the remote are the unit tests in `result`.
//!
//! The guards to mutate, in `src/result.rs` `talk_say`: the dry-run arm (it says the talk was
//! sent), the `sent` arm (it names no one), and the read of `detail.talk` that makes a result a
//! talk's (every talk says "Done" again).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::collections::HashSet;

use serde_json::Value;
use support::talk::{TalkCase, from_now, hosted_talk_seeded};

/// The talk host, with talk turned on in main's policy.
fn talk_on() -> TalkCase {
    hosted_talk_seeded(|test_box, a| {
        test_box.seed_trunk(
            a,
            &[
                ("src/a.rs", "fn a() {}\n"),
                (
                    ".bay/policy.toml",
                    "revision = 1\nunchecked = true\n\n[talk]\nenabled = true\n",
                ),
            ],
        );
    })
}

fn say(result: &Value) -> String {
    result["say"].as_str().unwrap_or_default().to_owned()
}

/// `bay talk <args>` as `actor`, which must be `ok`: its sentence, after its state is `state`.
fn ok(case: &TalkCase, actor: &str, args: &[&str], state: Option<&str>) -> (Value, String) {
    let (code, result) = case.talk(actor, args);
    assert_eq!(code, 0, "{args:?}: {result}");
    assert_eq!(result["decision"], "ok", "{result}");
    if let Some(state) = state {
        assert_eq!(result["state"], state, "{args:?}: {result}");
    }
    let said = say(&result);
    assert!(
        !said.contains("Done; nothing else is needed."),
        "{args:?} says it is done: {result}"
    );
    (result, said)
}

#[test]
fn every_talk_state_says_its_own_sentence() {
    let case = talk_on();
    let follow_up = from_now(3600);
    let ask = [
        "agent-b",
        "Pick a colour",
        "--kind",
        "discovery",
        "--decision",
        "colour",
        "--follow-up",
        follow_up.as_str(),
    ];
    let mut dry = ask.to_vec();
    dry.push("--dry-run");
    let (dry, dry_say) = ok(&case, "agent", &dry, None);
    assert_eq!(dry["detail"]["dry_run"], true, "{dry}");
    assert!(dry_say.contains("Nothing was sent"), "{dry_say}");

    // A send names who owes the reply and by when.
    let (sent, sent_say) = ok(&case, "agent", &ask, Some("sent"));
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    assert!(sent_say.contains("agent-b"), "{sent_say}");
    assert!(sent_say.contains(&follow_up), "{sent_say}");
    let (_, again_say) = ok(&case, "agent", &ask, Some("already_sent"));
    assert!(again_say.contains("agent-b"), "{again_say}");

    // An answer says the sender is told; a repeat, that nothing new was sent.
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    let answer = ["--answer", id.as_str(), "--value", "blue"];
    let (_, answered_say) = ok(&case, "agent-b", &answer, Some("answered"));
    assert!(answered_say.contains("agent"), "{answered_say}");
    let (_, repeat_say) = ok(&case, "agent-b", &answer, Some("already_answered"));

    // A backbrief says its status word; a repeat, that nothing new was sent.
    let (code, delivery) = case.talk(
        "agent",
        &[
            "agent-b",
            "Tag the release",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{delivery}");
    let delivery = delivery["detail"]["talk"].as_str().unwrap().to_owned();
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    let brief = ["--backbrief", delivery.as_str(), "ready"];
    let (_, brief_say) = ok(&case, "agent-b", &brief, Some("backbriefed"));
    assert!(brief_say.contains("ready"), "{brief_say}");
    assert!(brief_say.contains("agent"), "{brief_say}");
    let (_, brief_again) = ok(&case, "agent-b", &brief, Some("already_backbriefed"));

    let all = [
        &dry_say,
        &sent_say,
        &again_say,
        &answered_say,
        &repeat_say,
        &brief_say,
        &brief_again,
    ];
    let distinct: HashSet<&String> = all.iter().copied().collect();
    assert_eq!(distinct.len(), all.len(), "{all:#?}");
    for sentence in all {
        assert!(sentence.split_whitespace().count() <= 25, "{sentence}");
    }
}

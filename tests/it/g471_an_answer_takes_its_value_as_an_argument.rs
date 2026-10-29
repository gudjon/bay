//! G471 (T5, the owner's probe of bay talk, 2026-10-28). An answer takes its value as an
//! argument.
//!
//! A send takes its action as an argument (`bay talk <who> "<what>"`); an answer took its value
//! only as `--value`, and the parser refused `bay talk --answer <id> <value>`. Now the argument
//! is the value, and `--value` stays and works as before. A value given both ways is
//! `TALK_REFUSED`, and nothing is written.
//!
//! The guards to mutate, in `src/talk/send.rs` `answer_value`: the refusal of a value given
//! twice (the argument is taken), and the read of the argument (it is not read, so the
//! argument answers nothing).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::talk::{TalkCase, from_now, hosted_talk_seeded, talk_ref};

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

/// A discovery talk from `agent` to `agent-b`, accepted by `agent-b`: its id.
fn asked(case: &TalkCase, what: &str, decision: &str) -> String {
    let follow_up = from_now(3600);
    let (code, sent) = case.talk(
        "agent",
        &[
            "agent-b",
            what,
            "--kind",
            "discovery",
            "--decision",
            decision,
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{sent}");
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    sent["detail"]["talk"].as_str().unwrap().to_owned()
}

/// The value of the answer entry the host holds at the tip of talk `id`.
fn answered_with(case: &TalkCase, id: &str) -> Value {
    let name = talk_ref("agent-b", id);
    let tip = case.host_tip(&name).unwrap();
    let path = case
        .files(None, &tip)
        .into_iter()
        .rfind(|file| file.ends_with("-answer.json"))
        .unwrap_or_else(|| panic!("no answer on {name}"));
    let entry: Value = serde_json::from_str(
        &case
            .test_box
            .git_bare(&case.host, &["show", &format!("{tip}:{path}")]),
    )
    .unwrap();
    entry["value"].clone()
}

#[test]
fn the_argument_is_the_value_and_value_stays() {
    let case = talk_on();
    let id = asked(&case, "Pick a colour", "colour");
    let name = talk_ref("agent-b", &id);

    // The value as an argument answers the talk.
    let (code, answer) = case.talk("agent-b", &["--answer", &id, "blue"]);
    assert_eq!(code, 0, "{answer}");
    assert_eq!(answer["state"], "answered", "{answer}");
    assert_eq!(answered_with(&case, &id), "blue");
    // The same value again, either way, is the same answer.
    let (code, again) = case.talk("agent-b", &["--answer", &id, "blue"]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["state"], "already_answered", "{again}");
    let (code, again) = case.talk("agent-b", &["--answer", &id, "--value", "blue"]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["state"], "already_answered", "{again}");

    // `--value` answers as before.
    let other = asked(&case, "Pick a size", "size");
    let (code, answer) = case.talk("agent-b", &["--answer", &other, "--value", "large"]);
    assert_eq!(code, 0, "{answer}");
    assert_eq!(answer["state"], "answered", "{answer}");
    assert_eq!(answered_with(&case, &other), "large");
    assert!(case.host_tip(&name).is_some());
}

#[test]
fn a_value_given_both_ways_is_refused_and_nothing_is_written() {
    let case = talk_on();
    let id = asked(&case, "Pick a colour", "colour");
    let name = talk_ref("agent-b", &id);
    let before = case.host_tip(&name);

    let (code, both) = case.talk("agent-b", &["--answer", &id, "blue", "--value", "red"]);
    assert_eq!(code, 10, "{both}");
    assert_eq!(both["reason"], "TALK_REFUSED", "{both}");
    let why = both["detail"]["why"].as_str().unwrap_or_default();
    assert!(why.contains("twice"), "{both}");
    assert_eq!(
        case.host_tip(&name),
        before,
        "a refused answer wrote on {name}"
    );
    // The same, with the same value both ways: still given twice.
    let (code, same) = case.talk("agent-b", &["--answer", &id, "blue", "--value", "blue"]);
    assert_eq!(code, 10, "{same}");
    assert_eq!(case.host_tip(&name), before);

    // No value at all names the argument form.
    let (code, none) = case.talk("agent-b", &["--answer", &id]);
    assert_eq!(code, 10, "{none}");
    let why = none["detail"]["why"].as_str().unwrap_or_default();
    assert!(
        why.contains(&format!("bay talk --answer {id} <value>")),
        "{none}"
    );
    assert_eq!(case.host_tip(&name), before);
}

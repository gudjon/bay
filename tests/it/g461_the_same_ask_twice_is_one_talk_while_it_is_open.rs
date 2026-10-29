//! G461 (bay 0.6.0, T15; oz-cli's sandbox on 0.5.42: "the same ask sent twice makes two talks,
//! and the recipient is told twice"). An ask equal to an OPEN talk from the same sender name to
//! the same `who` (the same kind, `what` and target) is that talk: `already_sent`, whatever the
//! session it comes from and whatever `follow_up` the clock gave it. A field given beside it
//! that differs (a context, an explicit `--follow-up`) is `TALK_REFUSED`, naming the open talk:
//! a talk's body never changes, and supersede changes it. A closed talk does not hold its ask:
//! asking again is a new talk, which names the closed one in `repeats`, so its id is new.
//!
//! Before 0.6.0 the id held the sender's session and `follow_up`: a second session, or a
//! two-word send a second later, made a second talk; and the same ask after the talk closed was
//! `TALK_REFUSED` for good (ruling T7).
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/talk/send.rs` `open_same`: return `None` (the second session's ask is a second talk).
//! - `src/talk/send.rs` `open_same`: drop `!talk.closed()` (an answered ask holds its repeat).
//! - `src/talk.rs` `same_ask`: compare `what` only (a different target is the same talk).
//! - `src/talk/send.rs` `held`: drop the body comparison (a different context is `already_sent`).
//! - `src/talk/send.rs` `past_closed`: return the body unchanged (the repeat is refused).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::plain_talk::Plain;
use support::talk::{from_now, talk_ref};

#[test]
fn the_same_ask_from_another_session_or_a_second_later_is_the_open_talk() {
    let case = Plain::approvals();
    let follow_up = from_now(3600);
    let what = "Pick the wire format";
    let ask = [
        "talk",
        "agent-b",
        what,
        "--kind",
        "discovery",
        "--decision",
        "wire-format",
        "--follow-up",
        follow_up.as_str(),
    ];
    let (code, first) = case.bay("agent", &ask);
    assert_eq!(code, 0, "{first}");
    assert_eq!(first["state"], "sent", "{first}");
    let id = first["detail"]["talk"].as_str().unwrap().to_owned();

    // Another session of the same actor: its id would differ, its ask does not.
    let (code, again) = case.bay_in("agent", "s-other", &ask);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["state"], "already_sent", "{again}");
    assert_eq!(again["detail"]["talk"], id.as_str(), "{again}");
    let mut dry = ask.to_vec();
    dry.push("--dry-run");
    let (code, dry) = case.bay_in("agent", "s-third", &dry);
    assert_eq!(code, 0, "{dry}");
    assert_eq!(dry["detail"]["talk"], id.as_str(), "{dry}");
    assert_eq!(case.remote_talks(), vec![talk_ref("agent-b", &id)]);

    // The two-word send a second later: its default follow-up moved with the clock.
    let two = "Pick the log format";
    let (code, first) = case.talk("agent", &["agent-b", two]);
    assert_eq!(code, 0, "{first}");
    let two_id = first["detail"]["talk"].as_str().unwrap().to_owned();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let (code, later) = case.talk("agent", &["agent-b", two]);
    assert_eq!(code, 0, "{later}");
    assert_eq!(later["state"], "already_sent", "{later}");
    assert_eq!(later["detail"]["talk"], two_id.as_str(), "{later}");

    // Another target, another recipient or another sender is another ask.
    let (code, other) = case.talk("agent", &["agent-b", two, "--decision", "log"]);
    assert_eq!(code, 0, "{other}");
    assert_eq!(other["state"], "sent", "{other}");
    let (code, to_c) = case.talk("agent", &["agent-c", two]);
    assert_eq!(code, 0, "{to_c}");
    assert_eq!(to_c["state"], "sent", "{to_c}");
    let (code, from_c) = case.talk("agent-c", &["agent-b", two]);
    assert_eq!(code, 0, "{from_c}");
    assert_eq!(from_c["state"], "sent", "{from_c}");
    assert_eq!(case.remote_talks().len(), 5);
}

#[test]
fn a_different_body_for_an_open_ask_is_refused_naming_the_open_talk() {
    let case = Plain::approvals();
    let what = "Pick the log format";
    let follow_up = from_now(3600);
    let ask = [
        "talk",
        "agent-b",
        what,
        "--kind",
        "discovery",
        "--decision",
        "log-format",
    ];
    let mut first_ask = ask.to_vec();
    first_ask.extend(["--follow-up", follow_up.as_str()]);
    let (code, first) = case.bay("agent", &first_ask);
    assert_eq!(code, 0, "{first}");
    let id = first["detail"]["talk"].as_str().unwrap().to_owned();
    let later = from_now(7200);
    for more in [
        &[
            "--follow-up",
            follow_up.as_str(),
            "--context",
            "asked again",
        ][..],
        &["--follow-up", later.as_str()][..],
    ] {
        let mut args = ask.to_vec();
        args.extend(more);
        let (code, refused) = case.bay_in("agent", "s-other", &args);
        assert_eq!(code, 10, "{more:?}: {refused}");
        assert_eq!(refused["reason"], "TALK_REFUSED", "{refused}");
        assert_eq!(refused["detail"]["talk"], id.as_str(), "{refused}");
        let why = refused["detail"]["why"].as_str().unwrap();
        assert!(why.contains(&format!("--supersedes {id}")), "{why}");
    }
    assert_eq!(case.remote_talks().len(), 1);
}

#[test]
fn a_closed_talk_does_not_hold_its_ask() {
    let case = Plain::approvals();
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
    let (code, first) = case.talk("agent", &ask);
    assert_eq!(code, 0, "{first}");
    let first = first["detail"]["talk"].as_str().unwrap().to_owned();
    let (code, answered) = case.talk("agent-b", &["--answer", &first, "--value", "blue"]);
    assert_eq!(code, 0, "{answered}");
    assert_eq!(answered["state"], "answered", "{answered}");

    // The same ask, the same session and follow-up: a new talk that names the closed one.
    let (code, second) = case.talk("agent", &ask);
    assert_eq!(code, 0, "{second}");
    assert_eq!(second["state"], "sent", "{second}");
    let second = second["detail"]["talk"].as_str().unwrap().to_owned();
    assert_ne!(second, first);
    let body = case.body(&talk_ref("agent-b", &second));
    assert_eq!(body["repeats"], first.as_str(), "{body}");

    // While the new one is open, it holds the ask.
    let (code, again) = case.talk("agent", &ask);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["state"], "already_sent", "{again}");
    assert_eq!(again["detail"]["talk"], second.as_str(), "{again}");

    // Once it closes too, the third ask names the second.
    let (code, _) = case.talk("agent-b", &["--answer", &second, "--value", "red"]);
    assert_eq!(code, 0);
    let (code, third) = case.talk("agent", &ask);
    assert_eq!(code, 0, "{third}");
    assert_eq!(third["state"], "sent", "{third}");
    let third = third["detail"]["talk"].as_str().unwrap().to_owned();
    let body = case.body(&talk_ref("agent-b", &third));
    assert_eq!(body["repeats"], second.as_str(), "{body}");
    assert_eq!(case.remote_talks().len(), 3);
}

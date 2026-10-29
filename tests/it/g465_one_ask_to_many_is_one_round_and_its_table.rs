//! G465 (bay 0.6.0, T2/U4). `bay talk --to <a> --to <b> "<what>"` sends one talk to each
//! recipient, and `--to-live` to every actor with a live session in this repository but the
//! sender: one result, `detail.talks` one item per recipient, all of one round, `detail.round`,
//! which each talk's body names. The round's id is the ask and its recipients, so the same ask to
//! the same recipients again is the same round, each talk `already_sent` while it is open.
//! Every refusal is judged for every recipient before any talk is sent. A round with more
//! recipients than `[talk] max_round` is `TALK_REFUSED`, naming the cap and the count.
//! `bay talk --round <id>` prints one row per recipient: who, the talk, its standing, and its
//! answer or backbrief status.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/talk/round.rs` `fan_out`: drop the `max_round` refusal.
//! - `src/talk/round.rs` `fan_out`: drop the judging pass, so each talk is sent as it is judged
//!   (the refused round sends its first talk).
//! - `src/talk/round.rs` `recipients`: drop the dedupe (the repeated `--to` is two items), or
//!   keep the sender among the live actors.
//! - `src/talk/round.rs` `rows`: keep only the talks another one repeats (the row shows the
//!   recipient's closed talk, not its newest).

// Each case runs the hook as a process with a payload on stdin, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::process::Stdio;

use serde_json::Value;
use support::plain_talk::{APPROVALS, Plain, TALK_ON};
use support::talk::talk_ref;

fn items(result: &Value) -> Vec<Value> {
    result["detail"]["talks"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("no detail.talks: {result}"))
}

fn who_of(items: &[Value]) -> Vec<String> {
    let mut who: Vec<String> = items
        .iter()
        .map(|item| item["who"].as_str().unwrap().to_owned())
        .collect();
    who.sort();
    who
}

#[test]
fn one_ask_to_many_is_one_round_with_a_row_per_recipient() {
    let case = Plain::approvals();
    let what = "Pick the wire format";
    let ask = [
        "--to", "agent-b", "--to", "agent-c", "--to", "agent-b", what,
    ];
    let mut dry = ask.to_vec();
    dry.push("--dry-run");
    let (code, dry) = case.talk("agent", &dry);
    assert_eq!(code, 0, "{dry}");
    assert_eq!(dry["detail"]["dry_run"], true, "{dry}");
    assert_eq!(who_of(&items(&dry)), ["agent-b", "agent-c"], "{dry}");
    assert_eq!(
        case.remote_talks(),
        Vec::<String>::new(),
        "a dry run sent a talk"
    );

    let (code, sent) = case.talk("agent", &ask);
    assert_eq!(code, 0, "{sent}");
    let round = sent["detail"]["round"].as_str().unwrap().to_owned();
    assert!(bay::talk::is_talk_id(&round), "{sent}");
    let talks = items(&sent);
    assert_eq!(who_of(&talks), ["agent-b", "agent-c"], "{sent}");
    for item in &talks {
        assert_eq!(item["state"], "sent", "{item}");
        let name = talk_ref(
            item["who"].as_str().unwrap(),
            item["talk"].as_str().unwrap(),
        );
        assert_eq!(case.body(&name)["round"], round.as_str(), "{name}");
    }

    // The same ask to the same recipients again is the same round, and the same talks.
    let (code, again) = case.talk("agent", &ask);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["detail"]["round"], round.as_str(), "{again}");
    for item in items(&again) {
        assert_eq!(item["state"], "already_sent", "{item}");
    }
    assert_eq!(case.remote_talks().len(), 2);

    // The table: one row per recipient, and an answer once there is one.
    let b_talk = talks.iter().find(|item| item["who"] == "agent-b").unwrap()["talk"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, _) = case.talk("agent-b", &["--answer", &b_talk, "--value", "json"]);
    assert_eq!(code, 0);
    let (code, table) = case.talk("agent", &["--round", &round]);
    assert_eq!(code, 0, "{table}");
    assert_eq!(table["detail"]["round"], round.as_str(), "{table}");
    let rows = items(&table);
    assert_eq!(who_of(&rows), ["agent-b", "agent-c"], "{table}");
    for row in &rows {
        assert!(row["standing"].is_string(), "{row}");
        let answer = if row["who"] == "agent-b" {
            Value::from("json")
        } else {
            Value::Null
        };
        assert_eq!(row["answer"], answer, "{row}");
    }
    // Asked again once agent-b's talk closed: agent-b's row is its newest talk of the round.
    let (code, later) = case.talk("agent", &ask);
    assert_eq!(code, 0, "{later}");
    let newest = items(&later)
        .into_iter()
        .find(|item| item["who"] == "agent-b")
        .unwrap();
    assert_eq!(newest["state"], "sent", "{newest}");
    let (_, table) = case.talk("agent", &["--round", &round]);
    let row = items(&table)
        .into_iter()
        .find(|row| row["who"] == "agent-b")
        .unwrap();
    assert_eq!(row["talk"], newest["talk"], "{table}");
    let (code, unknown) = case.talk("agent", &["--round", "aaaaaaaaaaaaaaaa"]);
    assert_eq!(code, 10, "{unknown}");
    assert_eq!(unknown["reason"], "TALK_REFUSED", "{unknown}");
}

#[test]
fn a_round_past_max_round_or_with_one_refused_recipient_sends_nothing() {
    let case = Plain::new(&format!("{TALK_ON}max_round = 1\n{APPROVALS}"));
    let (code, refused) = case.talk("agent", &["--to", "agent-b", "--to", "agent-c", "Pick"]);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "TALK_REFUSED", "{refused}");
    assert_eq!(refused["detail"]["max_round"], 1, "{refused}");
    assert_eq!(refused["detail"]["recipients"], 2, "{refused}");
    let why = refused["detail"]["why"].as_str().unwrap();
    assert!(why.contains("max_round") && why.contains('2'), "{why}");
    assert_eq!(case.remote_talks(), Vec::<String>::new());

    // One recipient the round cannot reach (a name no actor can have) refuses the round whole.
    let case = Plain::approvals();
    let (code, refused) = case.talk("agent", &["--to", "agent-b", "--to", "Agent-C", "Pick"]);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "TALK_REFUSED", "{refused}");
    assert_eq!(refused["detail"]["who"], "Agent-C", "{refused}");
    assert_eq!(
        case.remote_talks(),
        Vec::<String>::new(),
        "a refused round sent a talk"
    );
}

/// A hook event on `a` for a session of `actor`: a live session on this machine.
fn live_session(case: &Plain, actor: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302/01-SessionStart.json");
    let mut payload: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    payload["session_id"] = Value::from(format!("s-{actor}"));
    payload["cwd"] = Value::from(case.a.display().to_string());
    let mut child = case
        .test_box
        .bay_process(&case.a)
        .env("BAY_ACTOR", actor)
        .args(["hook", "session.start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
}

#[test]
fn to_live_asks_every_live_actor_but_the_sender() {
    let case = Plain::approvals();
    for actor in ["agent", "agent-b", "agent-c"] {
        live_session(&case, actor);
    }
    let (code, sent) = case.talk("agent", &["--to-live", "Who holds the parser?"]);
    assert_eq!(code, 0, "{sent}");
    assert_eq!(who_of(&items(&sent)), ["agent-b", "agent-c"], "{sent}");
    assert!(sent["detail"]["round"].is_string(), "{sent}");
}

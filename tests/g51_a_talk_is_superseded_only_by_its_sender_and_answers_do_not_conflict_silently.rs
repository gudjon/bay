//! G51. A talk is superseded only by its sender, and answers do not conflict silently (`DESIGN`
//! § 11c; rulings T1 and T2 in `plans/wave2.md`).
//!
//! Built so far: a supersede by a third actor is refused (`TALK_SUPERSEDE_REFUSED`), by that
//! actor's bay and by the host; the original sender's supersede is accepted, and the recipient's
//! accept closes the old talk as `superseded`, naming its successor; a second, different answer to
//! a discovery talk without `supersedes` naming the standing answer is refused
//! (`TALK_ANSWER_CONFLICT`), by the answering bay and by the host; one that names it stands. A
//! talk takes one successor and a closed one none; a successor pushes a predecessor the host
//! does not hold yet in the same push, and the host judges it against that push. The
//! recipient's `SessionStart` notice naming the replaced talk comes with the hook notices.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use serde_json::{Value, json};
use support::talk::{TalkCase, from_now, hosted_talk, talk_ref};

fn discovery(case: &TalkCase, actor: &str, what: &str, more: &[&str]) -> (i32, Value) {
    // One follow-up time for every talk of a test, so the same ask is the same id.
    static FOLLOW_UP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let follow_up = FOLLOW_UP.get_or_init(|| from_now(3600));
    let mut args = vec![
        "agent-b",
        what,
        "--kind",
        "discovery",
        "--decision",
        "wire-format",
        "--follow-up",
        follow_up,
    ];
    args.extend(more);
    case.talk(actor, &args)
}

fn entry(case: &TalkCase, rev: &str, file: &str) -> Value {
    serde_json::from_str(
        &case
            .test_box
            .git_bare(&case.host, &["show", &format!("{rev}:{file}")]),
    )
    .unwrap()
}

#[test]
fn only_the_sender_supersedes_and_the_recipient_closes_the_old_talk() {
    let case = hosted_talk();
    let (code, first) = discovery(&case, "agent", "Pick the wire format: json or cbor", &[]);
    assert_eq!(code, 0, "{first}");
    let old = first["detail"]["talk"].as_str().unwrap().to_owned();

    // A third actor cannot supersede it: its bay refuses ...
    case.test_box.git_ok(
        &case.c,
        &["fetch", "-q", "origin", "refs/bay/talk/*:refs/bay/talk/*"],
    );
    let (code, third) = discovery(&case, "agent-c", "Pick cbor", &["--supersedes", &old]);
    assert_eq!(code, 10, "{third}");
    assert_eq!(third["reason"], "TALK_SUPERSEDE_REFUSED", "{third}");

    // ... and so does the host, when the commit skips that bay.
    let (code, dry) = case.talk(
        "agent-c",
        &[
            "agent-b",
            "Pick cbor",
            "--kind",
            "discovery",
            "--decision",
            "wire-format",
            "--follow-up",
            &from_now(3600),
            "--dry-run",
        ],
    );
    assert_eq!(code, 0, "{dry}");
    let mut body = dry["detail"]["body"].clone();
    body["supersedes"] = json!(old.as_str());
    body["id"] = json!(bay::talk::talk_id(&body).unwrap());
    let git = bay::git::Git::new(&case.c);
    let forged = bay::talk::root_commit(&git, &body, &case.c_key, "agent-c").unwrap();
    let name = talk_ref("agent-b", body["id"].as_str().unwrap());
    let stderr = case.refused(&case.c, &format!("{forged}:{name}"));
    assert!(stderr.contains("TALK_SUPERSEDE_REFUSED"), "{stderr}");

    // The sender's own supersede is accepted.
    let (code, second) = discovery(
        &case,
        "agent",
        "Pick the wire format: json, cbor or msgpack",
        &["--supersedes", &old],
    );
    assert_eq!(code, 0, "{second}");
    let new = second["detail"]["talk"].as_str().unwrap().to_owned();

    // A talk takes one successor: a second one is refused by the sender's bay and by the host.
    let (code, twice) = discovery(
        &case,
        "agent",
        "Pick cbor after all",
        &["--supersedes", &old],
    );
    assert_eq!(code, 10, "{twice}");
    assert_eq!(twice["reason"], "TALK_SUPERSEDE_REFUSED", "{twice}");
    // The sender's bay refuses it before a commit, not only the host.
    let why = twice["detail"]["why"].as_str().unwrap();
    assert!(
        why.contains(&new) && !why.contains("the host refused"),
        "{twice}"
    );
    let (code, dry) = discovery(&case, "agent", "Pick cbor after all", &["--dry-run"]);
    assert_eq!(code, 0, "{dry}");
    let mut body = dry["detail"]["body"].clone();
    body["supersedes"] = json!(old.as_str());
    body["id"] = json!(bay::talk::talk_id(&body).unwrap());
    let git = bay::git::Git::new(&case.a);
    let forged = bay::talk::root_commit(&git, &body, &case.keys.agent, "agent").unwrap();
    let twice_name = talk_ref("agent-b", body["id"].as_str().unwrap());
    let stderr = case.refused(&case.a, &format!("{forged}:{twice_name}"));
    assert!(
        stderr.contains("TALK_SUPERSEDE_REFUSED") && stderr.contains(&new),
        "{stderr}"
    );

    // The recipient's accept acks both and closes the old one as superseded by the new one.
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    let accept = &status["detail"]["talk_accept"];
    let mut acked: Vec<&str> = accept["acked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap())
        .collect();
    acked.sort_unstable();
    let mut both = vec![old.as_str(), new.as_str()];
    both.sort_unstable();
    assert_eq!(acked, both, "{status}");
    assert_eq!(
        accept["closed"],
        json!([{"talk": old, "state": "superseded", "by": new}]),
        "{status}"
    );
    let tip = case.host_tip(&talk_ref("agent-b", &old)).unwrap();
    let close = entry(&case, &tip, "entries/0002-close.json");
    assert_eq!(close["state"], "superseded", "{close}");
    assert_eq!(close["by"], new.as_str(), "{close}");
    assert_eq!(close["actor"]["name"], "agent-b", "{close}");

    // Closed means closed: the same ask again is `TALK_CLOSED`, naming the id.
    let (code, repeat) = discovery(&case, "agent", "Pick the wire format: json or cbor", &[]);
    assert_eq!(code, 10, "{repeat}");
    assert_eq!(repeat["reason"], "TALK_CLOSED", "{repeat}");
    assert_eq!(repeat["detail"]["talk"], old.as_str(), "{repeat}");
    // A repeat that differs only outside the ask is still that closed talk.
    let (code, repeat) = discovery(
        &case,
        "agent",
        "Pick the wire format: json or cbor",
        &["--context", "asked again"],
    );
    assert_eq!(code, 10, "{repeat}");
    assert_eq!(repeat["reason"], "TALK_CLOSED", "{repeat}");
    // A closed talk takes no successor either.
    let (code, late) = discovery(&case, "agent", "Pick msgpack", &["--supersedes", &old]);
    assert_eq!(code, 10, "{late}");
    assert_eq!(late["reason"], "TALK_SUPERSEDE_REFUSED", "{late}");
    let why = late["detail"]["why"].as_str().unwrap();
    assert!(
        why.contains(&format!("talk {old} is closed")) && !why.contains("the host refused"),
        "{late}"
    );
}

/// A successor sent while its predecessor is still only here takes the predecessor with it: the
/// host judges a supersede against what the same push brings, too.
#[test]
fn a_successor_pushes_a_predecessor_the_host_does_not_hold() {
    let case = hosted_talk();
    let host = case.host.to_str().unwrap().to_owned();
    let gone = case.test_box.path().join("unreachable.git");
    case.test_box.git_ok(
        &case.a,
        &["remote", "set-url", "origin", gone.to_str().unwrap()],
    );
    let (code, first) = discovery(&case, "agent", "Pick the log format", &[]);
    assert_eq!(code, 0, "{first}");
    assert_eq!(first["state"], "not_pushed", "{first}");
    let old = first["detail"]["talk"].as_str().unwrap().to_owned();
    let (code, second) = discovery(
        &case,
        "agent",
        "Pick the log format: text or json",
        &["--supersedes", &old],
    );
    assert_eq!(code, 0, "{second}");
    assert_eq!(second["state"], "not_pushed", "{second}");
    let new = second["detail"]["talk"].as_str().unwrap().to_owned();

    // Back online, the same ask again pushes the successor and the predecessor in one push.
    case.test_box
        .git_ok(&case.a, &["remote", "set-url", "origin", &host]);
    let (code, again) = discovery(
        &case,
        "agent",
        "Pick the log format: text or json",
        &["--supersedes", &old],
    );
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["state"], "already_sent", "{again}");
    assert!(case.host_tip(&talk_ref("agent-b", &new)).is_some());
    assert!(case.host_tip(&talk_ref("agent-b", &old)).is_some());
}

#[test]
fn a_second_different_answer_needs_to_supersede_the_first() {
    let case = hosted_talk();
    let (code, sent) = discovery(
        &case,
        "agent",
        "Pick the wire format",
        &["--decides", "agent-c"],
    );
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let name = talk_ref("agent-b", &id);

    let (code, answered) = case.talk("agent-b", &["--answer", &id, "--value", "json"]);
    assert_eq!(code, 0, "{answered}");
    let standing = answered["detail"]["answer"].as_str().unwrap().to_owned();
    assert_eq!(case.host_tip(&name).as_deref(), Some(standing.as_str()));

    // The same answer again is the same answer.
    let (code, again) = case.talk("agent-b", &["--answer", &id, "--value", "json"]);
    assert_eq!(code, 0, "{again}");
    assert_eq!(case.host_tip(&name).as_deref(), Some(standing.as_str()));

    // `decides`, on its own machine, gives a different value without superseding: refused.
    let (code, conflict) = case.talk("agent-c", &["--answer", &id, "--value", "cbor"]);
    assert_eq!(code, 10, "{conflict}");
    assert_eq!(conflict["reason"], "TALK_ANSWER_CONFLICT", "{conflict}");
    assert_eq!(conflict["detail"]["standing"]["commit"], standing.as_str());
    assert_eq!(conflict["detail"]["standing"]["value"], "json");
    assert_eq!(case.host_tip(&name).as_deref(), Some(standing.as_str()));

    // The host refuses the same conflict when the commit skips that bay.
    let git = bay::git::Git::new(&case.c);
    case.test_box.git_ok(
        &case.c,
        &["fetch", "-q", "origin", &format!("+{name}:{name}")],
    );
    let answer = |value: &str, supersedes: Option<&str>| {
        let mut entry = json!({"schema": "bay.talk/1", "entry": "answer", "talk": id,
            "actor": {"name": "agent-c", "session": "s-c"}, "value": value, "at": from_now(0)});
        if let Some(commit) = supersedes {
            entry["supersedes"] = json!(commit);
        }
        bay::talk::entry_commit(&git, &standing, &entry, &case.c_key, "agent-c").unwrap()
    };
    let forged = answer("cbor", None);
    let stderr = case.refused(&case.c, &format!("{forged}:{name}"));
    assert!(stderr.contains("TALK_ANSWER_CONFLICT"), "{stderr}");

    // A third actor, neither `who` nor `decides`, cannot answer: not by its bay, not at the host.
    let (code, third) = case.talk("agent", &["--answer", &id, "--value", "cbor"]);
    assert_eq!(code, 10, "{third}");
    let a_git = bay::git::Git::new(&case.a);
    case.test_box.git_ok(
        &case.a,
        &["fetch", "-q", "origin", &format!("+{name}:{name}")],
    );
    let by_third = json!({"schema": "bay.talk/1", "entry": "answer", "talk": id,
        "actor": {"name": "agent", "session": "s-agent"}, "value": "cbor",
        "supersedes": standing, "at": from_now(0)});
    let forged =
        bay::talk::entry_commit(&a_git, &standing, &by_third, &case.keys.agent, "agent").unwrap();
    let stderr = case.refused(&case.a, &format!("{forged}:{name}"));
    assert!(
        stderr.contains("only agent-b or `decides` answers talk"),
        "{stderr}"
    );

    // Superseding the standing answer is how a decision changes.
    let (code, changed) = case.talk(
        "agent-c",
        &[
            "--answer",
            &id,
            "--value",
            "cbor",
            "--supersedes",
            &standing,
        ],
    );
    assert_eq!(code, 0, "{changed}");
    let tip = case.host_tip(&name).unwrap();
    let files = case.files(None, &tip);
    let latest = files
        .iter()
        .rfind(|file| file.ends_with("-answer.json"))
        .unwrap();
    let latest = entry(&case, &tip, latest);
    assert_eq!(latest["value"], "cbor", "{latest}");
    assert_eq!(latest["supersedes"], standing.as_str(), "{latest}");
}

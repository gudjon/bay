//! G447 (A5, a fleet's sandbox run of 0.5.39). The sender of a talk hears its replies in session,
//! each once.
//!
//! A talk's ack, backbriefs, answer and close are entries its recipient writes on the talk's
//! chain, and the sender's poll fetches them. Its session was told none of them: only `bay status`
//! and `bay watch` read them. Now a hook tells each new entry once, in bay's words. Once is per
//! actor on the machine: a session that starts later is not told it again.
//!
//! The guards to mutate, all in `src/talk/replies.rs`: the told-keys filter in `pending` (an
//! entry told again when other talk refs move), the refs digest compare in `pending` (nothing
//! told after the first look), and the write in `mark` (told again at the next prompt).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use serde_json::Value;
use support::talk::{TalkCase, from_now, hosted_talk};

fn fixture(name: &str, session: &str, cwd: &Path) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302")
        .join(name);
    let mut payload: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    payload["session_id"] = Value::from(session);
    payload["cwd"] = Value::from(cwd.display().to_string());
    payload
}

/// `bay hook <event>` as `actor` on its clone, for `session`: what it printed. The talk poll is
/// made due first, so each prompt polls.
fn hook(case: &TalkCase, actor: &str, session: &str, event: &str, name: &str) -> String {
    let clone = case.clone_of(actor);
    let _ = std::fs::remove_file(clone.join(".git/bay/talk-poll.json"));
    let payload = fixture(name, session, clone);
    let mut child = case
        .test_box
        .bay_process(clone)
        .env("BAY_ACTOR", actor)
        .args(["hook", event])
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
    format!(
        "{}{}",
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap()
    )
}

fn prompt(case: &TalkCase, actor: &str, session: &str) -> String {
    hook(
        case,
        actor,
        session,
        "turn.start",
        "02-UserPromptSubmit.json",
    )
}

/// A talk from `agent-c` to `agent`: its id.
fn send(case: &TalkCase, args: &[&str]) -> String {
    let follow_up = from_now(3600);
    let mut all = vec!["agent", "Pick the wire format"];
    all.extend_from_slice(args);
    all.extend_from_slice(&["--follow-up", &follow_up]);
    let (code, sent) = case.talk("agent-c", &all);
    assert_eq!(code, 0, "{sent}");
    sent["detail"]["talk"].as_str().unwrap().to_owned()
}

#[test]
fn the_sender_hears_the_ack_and_the_answer_once() {
    let case = hosted_talk();
    let id = send(&case, &["--kind", "discovery", "--decision", "wire-format"]);
    // Nothing has come back yet.
    let first = prompt(&case, "agent-c", "s-c");
    assert!(!first.contains("your talk"), "{first}");

    // The recipient accepts (its ack) and answers, which closes the discovery talk.
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    let (code, answered) = case.talk("agent", &["--answer", &id, "--value", "json"]);
    assert_eq!(code, 0, "{answered}");

    let told = prompt(&case, "agent-c", "s-c");
    let read = format!("read it with `git log -p refs/bay/talk/agent/{id}`");
    assert!(
        told.contains(&format!("bay: your talk {id} to agent arrived: {read}")),
        "the sender was not told its talk arrived: {told}"
    );
    assert!(
        told.contains(&format!(
            "bay: your talk {id} to agent is answered with json: {read}"
        )),
        "the sender was not told of the answer: {told}"
    );
    // H2: the value is a short plain token, so the line shows it; text never does (unit test).
    // Each once.
    let again = prompt(&case, "agent-c", "s-c");
    assert!(!again.contains(&id), "{again}");
    // Another talk moves the talk refs: what was told stays told.
    let other = send(&case, &["--kind", "delivery", "--change", "abcd2345"]);
    let later = prompt(&case, "agent-c", "s-c");
    assert!(!later.contains(&id), "{later}");
    assert!(
        !later.contains(&other),
        "nothing came back on the new talk yet: {later}"
    );
}

#[test]
fn a_session_that_starts_is_not_told_the_replies_already_there() {
    let case = hosted_talk();
    let id = send(&case, &["--kind", "delivery", "--change", "abcd2345"]);
    let (code, status) = case.status("agent");
    assert_eq!(code, 0, "{status}");
    // One session hears the ack.
    let heard = prompt(&case, "agent-c", "s-c");
    assert!(
        heard.contains(&format!("bay: your talk {id} to agent arrived")),
        "{heard}"
    );
    // A session that starts after it is not told it again.
    let started = hook(
        &case,
        "agent-c",
        "s-c2",
        "session.start",
        "01-SessionStart.json",
    );
    assert!(!started.contains(&id), "{started}");
    let next = prompt(&case, "agent-c", "s-c2");
    assert!(
        !next.contains(&id),
        "a new session was told the past: {next}"
    );
}

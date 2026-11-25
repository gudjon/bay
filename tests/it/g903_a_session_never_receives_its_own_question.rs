//! G903 (a fleet finding, 2026-11-25: a session asked a person two questions; its own accept
//! acked each, and the wake gave each back to the session as a peer's ask; one was acked and
//! woken eight minutes after it had closed). The session that asks a person a question never acks
//! it or wakes on it: it waits for the answer in its own dialog. A closed talk is acked by no
//! session. Another session of the same actor still receives an open question.
//!
//! Guards to mutate: in `src/talk/accept.rs` `accept`, drop `asked_here` (the asking session
//! acks its own question), or drop `talk.closed()` (a closed question is acked).

use crate::support;

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::result_json;
use support::talk::{TalkCase, hosted_talk};

/// A recorded Claude Code payload `name` of the fixture folder `dir`, for `session` in `cwd`.
fn fixture(dir: &str, name: &str, session: &str, cwd: &Path) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
        .join(name);
    let mut payload: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    payload["session_id"] = json!(session);
    payload["cwd"] = json!(cwd);
    payload
}

/// `bay hook <event>` of `payload` as `agent-b` on its clone.
fn hook(case: &TalkCase, event: &str, payload: &Value) {
    let mut child = case
        .test_box
        .bay_process(&case.b)
        .env("BAY_ACTOR", "agent-b")
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
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

/// `agent-b`'s session `session` asks a person one question: the talk's id and body.
fn asked(case: &TalkCase, session: &str) -> (String, Value) {
    let ask = fixture(
        "claude-code-2.1.310",
        "01-PreToolUse-AskUserQuestion.json",
        session,
        &case.b,
    );
    hook(case, "tool.ask", &ask);
    let talks = case.test_box.git_bare(
        &case.host,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/bay/talk/agent-b/",
        ],
    );
    let talks: Vec<&str> = talks.lines().collect();
    assert_eq!(talks.len(), 1, "the host holds no question talk: {talks:?}");
    let body: Value = serde_json::from_str(
        &case
            .test_box
            .git_bare(&case.host, &["show", &format!("{}:talk.json", talks[0])]),
    )
    .unwrap();
    assert_eq!(body["from"]["session"], session, "{body}");
    (body["id"].as_str().unwrap().to_owned(), body)
}

/// `bay status` as `agent-b` in `session`, outside the poll interval: the talks it acked.
fn acked_in(case: &TalkCase, session: &str) -> Vec<String> {
    let (_, status) = result_json(
        case.test_box
            .bay(&case.b)
            .env("BAY_ACTOR", "agent-b")
            .env("BAY_SESSION", session)
            .env("BAY_TEST_TALK_COMMAND_UNBOUNDED", "1")
            .arg("status"),
    );
    let accept = &status["detail"]["talk_accept"];
    assert_eq!(accept["state"], "accepted", "{status}");
    accept["acked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn the_session_that_asks_never_acks_its_own_question_and_another_session_does() {
    // GIVEN agent-b's session s-q asked a person a question, still open
    let case = hosted_talk();
    let (id, _) = asked(&case, "s-q");

    // WHEN s-q accepts its actor's talks, THEN its own question is not acked there
    let acked = acked_in(&case, "s-q");
    assert!(
        !acked.contains(&id),
        "the asking session acked its own question: {acked:?}"
    );

    // AND another session of agent-b still receives it
    let acked = acked_in(&case, "s-other");
    assert_eq!(
        acked,
        vec![id],
        "another session did not receive the question"
    );
}

#[test]
fn a_question_that_closed_is_acked_by_no_session() {
    // GIVEN agent-b's session s-q asked a person a question, and went on, so the question ended
    let case = hosted_talk();
    let (id, _) = asked(&case, "s-q");
    let prompt = fixture(
        "claude-code-2.1.302",
        "02-UserPromptSubmit.json",
        "s-q",
        &case.b,
    );
    hook(&case, "turn.start", &prompt);
    let closed = case.files(None, &format!("refs/bay/talk/agent-b/{id}"));
    assert!(
        closed.iter().any(|path| path.ends_with("-close.json")),
        "the question stayed open: {closed:?}"
    );

    // WHEN another session of agent-b accepts its actor's talks
    let acked = acked_in(&case, "s-other");

    // THEN the closed question is not acked, so it wakes no one
    assert!(
        !acked.contains(&id),
        "a closed question was acked: {acked:?}"
    );
    let entries = case.files(None, &format!("refs/bay/talk/agent-b/{id}"));
    assert!(
        !entries.iter().any(|path| path.ends_with("-ack.json")),
        "{entries:?}"
    );
}

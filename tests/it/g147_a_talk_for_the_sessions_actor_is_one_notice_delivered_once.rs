//! G147. When a talk arrives for the session's actor, the session's agent gets one notice, once
//! (`plans/wave2.md` G147, ruling R13; `DESIGN` § 11c; like the `check_failed` notice of G23).
//!
//! A talk has arrived when the recipient's accept acked it (a fetching command, `bay status`).
//! The hook reads the talk refs this repository holds, no fetch, and tells the session's own
//! agent at its next `post-tool` or `user-prompt-submit`: one line naming the talk's id, its kind
//! and where to read it, never its prose. The delivered-notice cursor keeps the talk ids, so the
//! same talk is never told again in that session. A talk fetched but not acked is not arrived.

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

/// `bay hook <event>` as `agent-b` on its clone: its stdout and stderr.
fn hook(case: &TalkCase, event: &str, name: &str) -> (String, String) {
    let payload = fixture(name, "s-b", &case.b);
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
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn post_tool(case: &TalkCase) -> String {
    hook(case, "post-tool", "08-PostToolBatch.json").1
}

fn last_line(case: &TalkCase) -> Value {
    let text = std::fs::read_to_string(case.b.join(".git/bay/sessions/s-b.jsonl")).unwrap();
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

#[test]
fn a_talk_that_arrives_is_one_notice_and_never_again() {
    let case = hosted_talk();
    hook(&case, "session-start", "01-SessionStart.json");
    let follow_up = from_now(3600);
    let (code, sent) = case.talk(
        "agent",
        &[
            "agent-b",
            "Review the parser change",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();

    // Fetched without an accept: not arrived, no notice. The live poll (G54) ran a moment ago,
    // so this hook does not accept it either.
    let now = bay::live::unix_now();
    std::fs::write(
        case.b.join(".git/bay/talk-poll.json"),
        format!("{{\"last\": {now}}}"),
    )
    .unwrap();
    case.test_box.git_ok(
        &case.b,
        &[
            "fetch",
            "-q",
            "origin",
            "refs/bay/talk/agent-b/*:refs/bay/talk/agent-b/*",
        ],
    );
    assert_eq!(post_tool(&case), "");

    // B's status accepts and acks it: the next step is told, once.
    let (code, status) = case.status("agent-b");
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["detail"]["talk_accept"]["acked"][0], id.as_str());
    let told = post_tool(&case);
    assert_eq!(
        told,
        format!(
            "bay: talk {id} (delivery) arrived for agent-b: read it with \
             `git show refs/bay/talk/agent-b/{id}:talk.json`\n"
        )
    );
    assert!(
        !told.contains("parser"),
        "the prose never reaches the notice"
    );
    assert_eq!(last_line(&case)["talk_notices"][0], id.as_str());

    // Never again: not at the next step, not at the next prompt.
    assert_eq!(post_tool(&case), "");
    assert_eq!(
        hook(&case, "user-prompt-submit", "02-UserPromptSubmit.json").0,
        ""
    );
    assert!(last_line(&case).get("talk_notices").is_none());
}

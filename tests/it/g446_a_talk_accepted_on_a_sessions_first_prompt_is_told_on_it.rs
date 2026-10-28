//! G446 (A4, a fleet's sandbox run of 0.5.39). A talk that a session's first prompt accepts is
//! told on that prompt.
//!
//! The prompt's hook polls the actor's talks, accepts and acks a new one, and then reads its
//! notices. A session that has no notice cursor yet (no `SessionStart` hook ran, or its cursor
//! was lost) started one there and told nothing, so the talk it had just accepted was told one
//! prompt later. Now the cursor that starts at a hook tells what that hook can already tell; the
//! past it baselines stays untold.
//!
//! The guard to mutate: the `Err` arm of the cursor read in `notice::deliver` (`src/notice.rs`),
//! which returned before telling anything.

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

/// `bay hook turn.start` as `agent-b` on its clone, for session `s-b`: what it printed.
fn prompt(case: &TalkCase) -> String {
    let payload = fixture("02-UserPromptSubmit.json", "s-b", &case.b);
    let mut child = case
        .test_box
        .bay_process(&case.b)
        .env("BAY_ACTOR", "agent-b")
        .args(["hook", "turn.start"])
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

#[test]
fn the_first_prompt_that_accepts_a_talk_tells_it() {
    let case = hosted_talk();
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
    // No SessionStart hook ran for s-b: its first hook is this prompt, which accepts the talk.
    let first = prompt(&case);
    assert!(
        first.contains(&format!(
            "bay: talk {id} (delivery) from agent arrived for agent-b, reply owed by {follow_up}"
        )),
        "the first prompt accepted the talk but did not tell it: {first}"
    );
    assert!(
        !first.contains("parser"),
        "the prose never reaches the notice"
    );
    // Told once: the next prompt says nothing of it.
    let second = prompt(&case);
    assert!(!second.contains(&id), "{second}");
}

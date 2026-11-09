//! G689 (a fleet finding, 2026-11-09: a Grok session stalled at "Running 4 stop hooks" at every
//! turn end, until it was stopped by hand). `bay hook talk-wait` waits up to an hour for talk,
//! and is safe only where the product runs it in the background (`asyncRewake`, G493). Grok runs
//! a repository's `.claude/settings.json` entries but drops `asyncRewake` and runs the entry in
//! the foreground, so its turn waited on the poll. A wait whose payload is a guest's (BACKLOG 52
//! H4: Grok's shape) ends at once, exit 0; a wait from Claude Code waits as before.
//!
//! Guard to mutate: in `src/agent_hook.rs` `dispatch`, the `translated.is_some()` return before
//! `talk_wait` (the Grok wait polls, and the first case times out).

#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use support::talk::{TalkCase, hosted_talk};

/// `bay hook talk-wait` as `agent-b` with `payload` on stdin, its poll second scaled to 40 ms.
fn talk_wait(case: &TalkCase, payload: &str) -> std::process::Child {
    let mut child = case
        .test_box
        .bay_process(case.clone_of("agent-b"))
        .env("BAY_ACTOR", "agent-b")
        .env("BAY_SESSION", "s-agent-b")
        .env("BAY_TEST_TALK_POLL_MS", "40")
        .args(["hook", "talk-wait"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    child
}

/// How the child ended, within `bound`.
fn ended_within(child: &mut std::process::Child, bound: Duration) -> Option<i32> {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// A Stop payload in Grok's recorded shape (`tests/fixtures/grok-1.0.52/Stop-0.json`).
fn grok_stop(case: &TalkCase) -> String {
    let root = case.clone_of("agent-b").display().to_string();
    let mut payload: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/grok-1.0.52/Stop-0.json"),
        )
        .unwrap(),
    )
    .unwrap();
    payload["cwd"] = root.clone().into();
    payload["workspaceRoot"] = root.into();
    payload.to_string()
}

#[test]
fn a_guests_talk_wait_ends_at_once() {
    let case = hosted_talk();
    // GIVEN Grok's Stop payload WHEN its talk-wait entry runs THEN it ends at once
    let mut wait = talk_wait(&case, &grok_stop(&case));
    let ended = ended_within(&mut wait, Duration::from_secs(10));
    if ended.is_none() {
        let _ = wait.kill();
        let _ = wait.wait();
    }
    assert_eq!(ended, Some(0), "Grok's turn end waited on the talk poll");
}

#[test]
fn a_claude_code_talk_wait_still_waits() {
    let case = hosted_talk();
    let payload = r#"{"hook_event_name":"Stop","session_id":"s-agent-b","stop_hook_active":false}"#;
    let mut wait = talk_wait(&case, payload);
    assert_eq!(
        ended_within(&mut wait, Duration::from_secs(2)),
        None,
        "Claude Code's background wait ended at once"
    );
    wait.kill().unwrap();
    let _ = wait.wait();
}

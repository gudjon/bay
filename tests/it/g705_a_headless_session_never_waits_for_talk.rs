//! G705 (a measurement, 2026-11-12: headless `claude -p` runs on one box were held 525 to 3,649 s
//! each, some 36 hours in six days). `bay hook talk-wait` waits up to an hour for talk, safe only
//! where the product runs it in the background (G493). Claude Code's headless entry
//! (`CLAUDE_CODE_ENTRYPOINT=sdk-cli`) waits for it like any hook, and no one waits there to be
//! woken: the wait ends at once, exit 0. An interactive session still waits (G689).
//!
//! Guard to mutate: in `src/agent_hook.rs` `dispatch`, the `headless()` return before
//! `talk_wait` (the headless wait polls, and the case times out).

#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use support::talk::hosted_talk;

#[test]
fn a_headless_session_never_waits_for_talk() {
    let case = hosted_talk();
    // GIVEN a headless Claude Code run WHEN its turn ends THEN its talk wait ends at once
    let mut child = case
        .test_box
        .bay_process(case.clone_of("agent-b"))
        .env("BAY_ACTOR", "agent-b")
        .env("BAY_SESSION", "s-agent-b")
        .env("BAY_TEST_TALK_POLL_MS", "40")
        .env("CLAUDE_CODE_ENTRYPOINT", "sdk-cli")
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
        .write_all(
            br#"{"hook_event_name":"Stop","session_id":"s-agent-b","stop_hook_active":false}"#,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut ended = None;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            ended = status.code();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if ended.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    assert_eq!(
        ended,
        Some(0),
        "a headless turn end waited on the talk poll"
    );
}

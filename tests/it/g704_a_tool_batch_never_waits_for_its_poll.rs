//! G704 (a measurement, 2026-11-12: a talk poll was due on about 40% of an agent's tool batches,
//! and with a slow remote each held its batch the full 3 s, some 9 minutes of an 83-minute
//! session). The hook after a tool batch (`step.end`) starts the poll that is due and never waits
//! for it; the poll's talks are told at a later hook (G147). A prompt's hook still waits for its
//! poll a short while (G632).
//!
//! Guard to mutate: in `src/hook_poll.rs` `join`, the `step.end` case (the batch waits `JOIN`).

#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::talk::{from_now, hosted_talk};

#[test]
fn a_tool_batch_never_waits_for_its_poll() {
    let case = hosted_talk();
    let follow_up = from_now(3600);
    // GIVEN a talk for agent-b, and a remote that takes 8 s to answer agent-b's clone
    let (code, sent) = case.talk(
        "agent",
        &[
            "agent-b",
            "Review the parser change",
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{sent}");
    case.test_box.git_ok(
        &case.b,
        &[
            "config",
            "remote.origin.uploadpack",
            "sleep 8; git upload-pack",
        ],
    );

    // WHEN agent-b's session ends a tool batch, with its talk poll due
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302/08-PostToolBatch.json");
    let mut payload: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    payload["session_id"] = Value::from("s-b");
    payload["cwd"] = Value::from(case.b.display().to_string());
    let started = Instant::now();
    let mut child = case
        .test_box
        .bay_process(&case.b)
        .env("BAY_ACTOR", "agent-b")
        .env("BAY_TEST_RELEASE_JOIN", "1")
        .args(["hook", "step.end"])
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
    let took = started.elapsed();

    // THEN the hook ends well before the 3 s a prompt waits for its poll
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        took < Duration::from_millis(2500),
        "the tool batch waited {took:?}"
    );
}

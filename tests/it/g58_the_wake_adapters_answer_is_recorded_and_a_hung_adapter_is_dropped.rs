//! G58 and G53: the wake adapter's answer is recorded on the ack, a hung adapter is dropped
//! after 5 s, and the sender sees the wake outcome after a fetch (ruling T4; the wake contract).
//!
//! The recipient's machine asks `[wake] command` once, at accept. A fake adapter that marks the
//! session blocked answers `skipped` / `SESSION_BLOCKED`; one that hangs is recorded `uncertain` /
//! `WAKE_ADAPTER_FAILED` within the bound, and the accept still writes the ack.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::talk::{TalkCase, from_now, hosted_talk_seeded};

const WAKE: &str = "#!/bin/sh\ncat >/dev/null\nif [ -e \"$HOME/../wake-hangs\" ]; then sleep 30; fi\nprintf '{\"schema\":\"bay.wake-result/1\",\"result\":\"skipped\",\"reason\":\"SESSION_BLOCKED\",\"at\":\"2026-10-20T00:00:00Z\"}'\n";

fn with_wake() -> TalkCase {
    hosted_talk_seeded(|test_box, a| {
        let script = a.join("scripts/bay-wake");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, WAKE).unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        test_box.seed_trunk(
            a,
            &[
                ("src/a.rs", "fn a() {}\n"),
                (
                    ".bay/config.toml",
                    "[wake]\ncommand = \"scripts/bay-wake\"\n",
                ),
                ("scripts/bay-wake", WAKE),
            ],
        );
    })
}

fn send(case: &TalkCase) -> String {
    let follow_up = from_now(3600);
    let (code, sent) = case.talk(
        "agent",
        &[
            "agent-b",
            "Rebase onto the new parser",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{sent}");
    sent["detail"]["talk"].as_str().unwrap().to_owned()
}

fn wake_of(status: &Value, id: &str) -> Value {
    status["detail"]["talks"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["talk"] == id))
        .map_or(Value::Null, |row| row["wake"].clone())
}

#[test]
fn a_blocked_session_is_recorded_skipped_and_the_sender_sees_it() {
    let case = with_wake();
    let id = send(&case);
    let (_, accepted) = case.status("agent-b");
    assert_eq!(
        wake_of(&accepted, &id),
        serde_json::json!({"result": "skipped", "reason": "SESSION_BLOCKED"}),
        "{accepted}"
    );
    // The sender sees it after its own fetch.
    let (_, status) = case.status("agent");
    assert_eq!(
        wake_of(&status, &id)["reason"],
        "SESSION_BLOCKED",
        "{status}"
    );
}

#[test]
fn a_hung_adapter_is_dropped_within_the_bound_and_the_ack_is_written() {
    let case = with_wake();
    let id = send(&case);
    let home = case.test_box.home.clone();
    std::fs::write(Path::new(&home).join("../wake-hangs"), "").unwrap();
    let started = Instant::now();
    let (_, accepted) = case.status("agent-b");
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        wake_of(&accepted, &id),
        serde_json::json!({"result": "uncertain", "reason": "WAKE_ADAPTER_FAILED"}),
        "{accepted}"
    );
}

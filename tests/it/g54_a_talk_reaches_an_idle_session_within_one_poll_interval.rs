//! G54: a talk reaches an idle session within one poll interval, and a live session does not
//! fetch twice inside one (ruling T5; `DESIGN` § 11c).
//!
//! The idle wait (`bay hook talk-wait`, run by an asynchronous hook) polls the actor's talks every
//! `BAY_TALK_POLL_S` and exits 2, the wake, at the first talk it accepts, with the line a
//! prompt's notice gives for it (H3: sender, reply time, the answer command). A live session's
//! hooks (`turn.start`, `step.end`) accept talks at most once per `BAY_TALK_POLL_S`. The
//! backoff (ten empty polls double the interval) is the unit test in `talk::poll`. A test build
//! scales the schedule's second by `BAY_TEST_TALK_POLL_MS`.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::process::Stdio;
use std::time::{Duration, Instant};

use support::talk::{TalkCase, from_now, hosted_talk};

fn send(case: &TalkCase, what: &str) -> String {
    let follow_up = from_now(3600);
    let (code, sent) = case.talk(
        "agent",
        &[
            "agent-b",
            what,
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

/// Whether the host holds an ack on `id`'s chain.
fn acked(case: &TalkCase, id: &str) -> bool {
    let name = support::talk::talk_ref("agent-b", id);
    case.host_tip(&name).is_some_and(|tip| {
        case.files(None, &tip)
            .iter()
            .any(|file| file.contains("-ack.json"))
    })
}

#[test]
fn an_idle_wait_wakes_on_the_first_talk_within_one_interval() {
    let case = hosted_talk();
    // 30 schedule seconds of 40 ms: one interval is 1.2 s.
    let mut wait = case
        .test_box
        .bay_process(case.clone_of("agent-b"))
        .env("BAY_ACTOR", "agent-b")
        .env("BAY_SESSION", "s-agent-b")
        .env("BAY_TEST_TALK_POLL_MS", "40")
        .args(["hook", "talk-wait"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let id = send(&case, "Rebase onto the new parser");
    let sent = Instant::now();
    let deadline = sent + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = wait.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "the idle wait never woke");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(2), "the wake is exit 2");
    // One interval, plus the time one accept takes.
    assert!(
        sent.elapsed() < Duration::from_secs(12),
        "{:?}",
        sent.elapsed()
    );
    assert!(acked(&case, &id), "the wait accepted it");
    // H3: the wake says what a prompt's notice says of the talk: id, kind, sender, reply time.
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut wait.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(
        stderr.contains(&format!(
            "bay: talk {id} (delivery) from agent arrived for agent-b, reply owed by "
        )),
        "{stderr}"
    );
    assert!(
        !stderr.contains("parser"),
        "the prose never reaches the line"
    );
}

#[test]
fn a_live_session_fetches_at_most_once_per_interval() {
    let case = hosted_talk();
    let hook = |event: &str| {
        let out = case
            .bay_as("agent-b")
            .args(["hook", event])
            .write_stdin("{}")
            .output()
            .unwrap();
        assert!(out.status.code().is_some(), "{out:?}");
    };
    hook("turn.start");
    let id = send(&case, "Rename the flag");
    // Inside the interval: no fetch, so no accept.
    hook("step.end");
    assert!(!acked(&case, &id), "a second fetch inside one interval");
    // The interval passed (the state file says the last fetch was long ago).
    let state = case.clone_of("agent-b").join(".git/bay/talk-poll.json");
    std::fs::write(&state, r#"{"last": 0}"#).unwrap();
    hook("step.end");
    assert!(
        acked(&case, &id),
        "the next hook after the interval accepted it"
    );
}

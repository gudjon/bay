//! G493 (I1, bay 0.6.0; oz-cli's sandbox finding, 2026-10-28: `bay hook talk-wait` woke an idle
//! session 31 s after a send, but nothing wired it). An idle seat waits for talk by itself.
//!
//! `bay install` writes a second `Stop` entry, `bay hook talk-wait`, as an `asyncRewake` hook:
//! the product runs it in the background at each stop, and its exit 2 wakes the agent with the
//! talk's line. The product starts it at every stop with no de-duplication, so one wait per
//! actor and clone polls and a second start ends at once. While the policy on main turns talk
//! off it ends at once too, so the entry costs nothing where talk is off.
//!
//! Guards to mutate: `src/agent_settings.rs` the `("Stop", "talk-wait")` row and the
//! `asyncRewake` field; `src/agent_hook.rs` `one_wait`; `src/talk/poll.rs` the `off` return.

#![cfg(unix)]

use crate::support;

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::TestBox;
use support::talk::{TalkCase, hosted_talk, hosted_talk_seeded};

const CLAUDE_ADAPTER: &str = "main = \"main\"\n\n[agent]\nproduct = \"claude\"\n";

fn settings(repo: &Path) -> Value {
    let text = std::fs::read_to_string(repo.join(".claude/settings.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn bay_install_wires_talk_wait_as_a_background_wake_at_each_stop() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.312"));
    test_box.bay_ok(&repo, &["install"]);
    let settings = settings(&repo);
    let hooks: Vec<&Value> = settings["hooks"]["Stop"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["hooks"].as_array().unwrap())
        .collect();
    let wait = hooks
        .iter()
        .find(|hook| {
            hook["command"]
                .as_str()
                .is_some_and(|command| command.ends_with("bay hook talk-wait"))
        })
        .unwrap_or_else(|| panic!("no talk-wait entry under Stop: {settings}"));
    assert_eq!(wait["asyncRewake"], true, "{wait}");
    assert_eq!(wait["timeout"], 3700, "{wait}");
    assert!(
        hooks.iter().any(|hook| hook["command"]
            .as_str()
            .is_some_and(|command| command.ends_with("bay hook turn.end"))),
        "the stop's own entry stays: {settings}"
    );
}

/// `bay hook talk-wait` as `agent-b`, its poll second scaled to 40 ms.
fn talk_wait(case: &TalkCase) -> std::process::Child {
    case.test_box
        .bay_process(case.clone_of("agent-b"))
        .env("BAY_ACTOR", "agent-b")
        .env("BAY_SESSION", "s-agent-b")
        .env("BAY_TEST_TALK_POLL_MS", "40")
        .args(["hook", "talk-wait"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
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

#[test]
fn one_wait_polls_and_a_second_start_ends_at_once() {
    let case = hosted_talk();
    let mut first = talk_wait(&case);
    std::thread::sleep(Duration::from_millis(500));
    let mut second = talk_wait(&case);
    assert_eq!(
        ended_within(&mut second, Duration::from_secs(10)),
        Some(0),
        "a second wait ends at once while the first one waits"
    );
    assert_eq!(
        first.try_wait().unwrap(),
        None,
        "the first wait still waits"
    );
    first.kill().unwrap();
    let _ = first.wait();
}

#[test]
fn a_wait_ends_at_once_while_main_turns_talk_off() {
    let case = hosted_talk_seeded(|test_box, a| {
        test_box.seed_trunk(
            a,
            &[
                ("src/a.rs", "fn a() {}\n"),
                (
                    ".bay/policy.toml",
                    &format!("{}\n[talk]\nenabled = false\n", support::UNCHECKED_POLICY),
                ),
            ],
        );
    });
    // A clone that never fetched the registry, where no accept can judge a talk: the wait reads
    // the policy before anything else.
    let fresh = case.test_box.clone_of(&case.host, "d");
    let mut wait = case
        .test_box
        .bay_process(&fresh)
        .env("BAY_ACTOR", "agent-b")
        .env("BAY_SESSION", "s-agent-b-fresh")
        .env("BAY_TEST_TALK_POLL_MS", "40")
        .args(["hook", "talk-wait"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert_eq!(
        ended_within(&mut wait, Duration::from_secs(10)),
        Some(0),
        "a wait while talk is off ends at once"
    );
}

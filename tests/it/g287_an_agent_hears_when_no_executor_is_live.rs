//! G287. An agent hears when no executor is live.
//!
//! When the repository has operations queued and no fresh executor presence, the rule of
//! `bay status`'s `no_live_executor` warning (G67), the agent hook tells the session one line at
//! `session-start` and `user-prompt-submit`: no executor is running, queued changes wait, and
//! `bay executor run` or `bay executor install` starts one. It is said at most once per session
//! for each change of that state: the session's notice cursor keeps that it was told, and an
//! executor that comes (or a queue that empties) arms it again. The session's line records it
//! (`no_executor_notice`).
//!
//! The guards to mutate: in `src/notice.rs`, `no_executor` returns early with
//! `if waiting == cursor.no_executor` (drop it, and the second prompt is told again) and stores
//! `cursor.no_executor = waiting` (drop it, and the cleared state never arms the line again);
//! `src/presence.rs` `no_live_executor` returns `None` when `any_fresh(&rows)` (drop it, and a
//! live executor is reported missing); `src/session.rs` `notices` calls `no_executor` at
//! `SessionStart` and at `UserPromptSubmit` (drop either, and that hook says nothing).

// Each case runs shell scripts (the git wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

const LINE: &str = "bay: no executor is running for this repository; queued changes wait. Start one with `bay executor run`, or install it with `bay executor install`.";

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

/// `bay hook <event>` for `session` in `cwd`, with the reader's presence clock `skew_s`
/// seconds on: its stdout and stderr together.
fn hook(test_box: &TestBox, cwd: &Path, session: &str, event: &str, skew_s: u64) -> String {
    let name = if event == "session-start" {
        "01-SessionStart.json"
    } else {
        "02-UserPromptSubmit.json"
    };
    let payload = fixture(name, session, cwd);
    let mut child = test_box
        .bay_process(cwd)
        .env("BAY_TEST_PRESENCE_CLOCK_SKEW_S", skew_s.to_string())
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
    format!(
        "{}{}",
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap()
    )
}

fn prompt(test_box: &TestBox, repo: &Path, session: &str, skew_s: u64) -> String {
    hook(test_box, repo, session, "user-prompt-submit", skew_s)
}

fn last_line(repo: &Path, session: &str) -> Value {
    let text =
        std::fs::read_to_string(repo.join(format!(".git/bay/sessions/{session}.jsonl"))).unwrap();
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

/// A repository set up for bay with no checks, its remote, and a clone an executor runs in.
fn seeded() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let executor = test_box.clone_of(&remote, "executor");
    (test_box, repo, executor)
}

/// Starts a change, edits it, and submits it: one operation queued.
fn queue_one(test_box: &TestBox, repo: &Path) {
    let (_, view) = test_box.start_and_edit(repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");
}

#[test]
fn queued_work_with_no_executor_is_told_once_per_state() {
    let (test_box, repo, executor) = seeded();
    // Nothing queued: nothing to tell.
    let started = hook(&test_box, &repo, "s-1", "session-start", 0);
    assert!(!started.contains(LINE), "{started}");
    assert!(!prompt(&test_box, &repo, "s-1", 0).contains(LINE));

    // Queued, and no executor ever seen: the next prompt is told, once.
    queue_one(&test_box, &repo);
    let told = prompt(&test_box, &repo, "s-1", 0);
    assert!(told.contains(LINE), "{told}");
    assert_eq!(last_line(&repo, "s-1")["no_executor_notice"], true);
    let again = prompt(&test_box, &repo, "s-1", 0);
    assert!(!again.contains(LINE), "told twice: {again}");
    assert!(last_line(&repo, "s-1").get("no_executor_notice").is_none());

    // A session that starts now hears it at its start.
    let other = hook(&test_box, &repo, "s-2", "session-start", 0);
    assert!(other.contains(LINE), "{other}");
    assert!(!prompt(&test_box, &repo, "s-2", 0).contains(LINE));

    // An executor runs and lands the work; a fetching command brings its presence and the
    // landed chain. Nothing waits, so nothing is told, and the cursor is armed again.
    let (code, ran) = result_json(test_box.bay(&executor).args(["executor", "run", "--once"]));
    assert_eq!(code, 0, "{ran}");
    let (code, status) = result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 0, "{status}");
    assert!(!prompt(&test_box, &repo, "s-1", 0).contains(LINE));

    // Work queues again while the executor's presence is fresh: it is live, nothing is told.
    queue_one(&test_box, &repo);
    let live = prompt(&test_box, &repo, "s-1", 0);
    assert!(
        !live.contains(LINE),
        "a live executor was reported missing: {live}"
    );
    // An hour on, the same presence is stale: the session is told again, once.
    let stale = prompt(&test_box, &repo, "s-1", 3600);
    assert!(stale.contains(LINE), "{stale}");
    assert!(!prompt(&test_box, &repo, "s-1", 3600).contains(LINE));
}

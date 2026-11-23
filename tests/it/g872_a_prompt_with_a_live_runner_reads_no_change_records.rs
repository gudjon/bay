//! G872 (a measurement, 2026-11-23: `bay hook turn.start` took 1.1 s at its median and 4.5 s
//! on a loaded box, in a store of 11,200 changes, and almost all of it read every change's
//! record to count the queued operations). The no-runner line (G287) is said only when
//! operations wait and no runner presence is fresh, so the hook asks the presence first and reads
//! the change records only when no presence is fresh: a prompt in a repository whose runner is
//! live lists no change refs. With the presence stale, the queue is read and the line is said as
//! before.
//!
//! Read from the trace a test build writes when `BAY_TEST_GIT_TRACE` names a file (G171).
//!
//! Guard to mutate: in `src/presence.rs` `unserved_queue`, the early `return false` when a
//! presence is fresh (drop it, and every prompt reads every record).

// Each case runs shell scripts (the git wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

const LINE: &str = "bay: no runner is running for this repository; queued changes wait.";

/// `bay hook <event>` for `session` in `cwd`, with the reader's presence clock `skew_s` seconds
/// on, its Git spawns traced to `trace`: its stdout and stderr together, and its pid.
fn hook(
    test_box: &TestBox,
    cwd: &Path,
    (session, event, skew_s): (&str, &str, u64),
    trace: &Path,
) -> (String, u32) {
    let name = if event == "session.start" {
        "01-SessionStart.json"
    } else {
        "02-UserPromptSubmit.json"
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302")
        .join(name);
    let mut payload: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    payload["session_id"] = Value::from(session);
    payload["cwd"] = Value::from(cwd.display().to_string());
    let mut child = test_box
        .bay_process(cwd)
        .env("BAY_TEST_PRESENCE_CLOCK_SKEW_S", skew_s.to_string())
        .env("BAY_TEST_GIT_TRACE", trace)
        .args(["hook", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    (
        format!(
            "{}{}",
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap()
        ),
        pid,
    )
}

/// The listings of every change ref (`for-each-ref ... refs/bay/changes/`) that process `pid`
/// ran, from `trace`.
fn change_listings(trace: &Path, pid: u32) -> usize {
    std::fs::read_to_string(trace)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| line["pid"] == pid && line["sub"] == "for-each-ref")
        .filter(|line| {
            line["args"]
                .as_array()
                .is_some_and(|args| args.iter().any(|arg| arg == "refs/bay/changes/"))
        })
        .count()
}

/// A repository set up for bay with no checks and a runner lease of 30 s, and a clone a runner
/// runs in.
fn seeded() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = unchecked_files();
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nlease_s = 30\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let executor = test_box.clone_of(&remote, "executor");
    (test_box, repo, executor)
}

#[test]
fn a_prompt_with_a_live_runner_reads_no_change_records() {
    let (test_box, repo, executor) = seeded();
    let trace = test_box.path().join("trace.jsonl");
    let (started, _) = hook(&test_box, &repo, ("s-1", "session.start", 0), &trace);
    assert!(!started.contains(LINE), "{started}");
    // GIVEN a runner whose presence the last fetch recorded, and one operation queued
    let (code, ran) = result_json(test_box.bay(&executor).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{ran}");
    let (code, status) = result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 0, "{status}");
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");

    // WHEN a prompt starts while that presence is fresh
    let (live, pid) = hook(&test_box, &repo, ("s-1", "turn.start", 0), &trace);
    // THEN nothing is said of a missing runner, and no change ref is listed
    assert!(
        !live.contains(LINE),
        "a live runner was reported missing: {live}"
    );
    assert_eq!(
        change_listings(&trace, pid),
        0,
        "a prompt with a live runner listed the change refs"
    );

    // AND an hour on, the same presence is stale: the queue is read and the line is said
    let (stale, pid) = hook(&test_box, &repo, ("s-1", "turn.start", 3600), &trace);
    assert!(stale.contains(LINE), "{stale}");
    assert!(change_listings(&trace, pid) > 0);
}

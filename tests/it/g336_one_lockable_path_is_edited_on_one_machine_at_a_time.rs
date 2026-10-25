//! G336 (BACKLOG 57, owner ruling 2026-10-23: "57 approved, go ahead"): one lockable path is
//! edited on one machine at a time.
//!
//! `[locks] paths` in `.bay/policy.toml` names paths that never merge (a binary, an image). At
//! `tool.before`, the first edit of one takes its lock on the remote, a create-only ref
//! `refs/bay/locks/<hash>`; on another machine, the same edit is refused `PATH_HELD`, naming the
//! holder. The holding session's own edits go through, its stop releases the lock, and a lock
//! past the takeover window is taken over. Offline, a lockable edit is refused
//! `REMOTE_UNREACHABLE`: unknown is never free. A bay host takes the lock refs.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/locks.rs` `take`: `take_one(&repo, &me, now)?` to `let _ = take_one(&repo, &me, now)`
//!   (the remote's answer is ignored, and both machines edit).
//! - `src/locks.rs` `take_one`: the held arm's `now.saturating_sub(lock.at_s) < window_s` to
//!   `true` (a lock never expires).
//! - `src/session.rs` `claims`: drop `crate::locks::release` at the stop (the lock outlives it).
//! - `src/locks.rs` `take_one`: drop the `lock.released` arm (a released lock still holds).
//! - `src/names.rs` `bay_ref`: drop the `locks` namespace (a bay host refuses the lock).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};
use support::{TestBox, UNCHECKED_POLICY};

/// The lockable art, with a takeover window of `2 * lease_s`.
fn policy_files(lease_s: u32) -> Vec<(String, String)> {
    vec![
        (
            ".bay/policy.toml".to_owned(),
            format!("{UNCHECKED_POLICY}\n[locks]\npaths = [\"art/\"]\n"),
        ),
        (
            ".bay/config.toml".to_owned(),
            format!("[runner]\nlease_s = {lease_s}\nheartbeat_s = 1\n"),
        ),
    ]
}

/// Two machines: the seeding clone and a second clone of one remote. Returns the box, both
/// clones and the remote.
fn two_machines(lease_s: u32) -> (TestBox, PathBuf, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let first = test_box.repo_any("repo");
    test_box.seed_with_check(
        &first,
        &[("art/logo.bin", "logo\n"), ("src/a.rs", "fn a() {}\n")],
        &policy_files(lease_s),
    );
    let remote = test_box.path().join("repo-remote.git");
    let second = test_box.clone_of(&remote, "second");
    (test_box, first, second, remote)
}

/// `bay hook <event>` with `payload` on stdin; returns (exit, stderr).
fn hook(test_box: &TestBox, cwd: &Path, event: &str, payload: &Value) -> (Option<i32>, String) {
    let mut child = test_box
        .bay_process(cwd)
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
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `tool.before` of `session` writing `file` in the clone `cwd`.
fn edit(test_box: &TestBox, session: &str, cwd: &Path, file: &str) -> (Option<i32>, String) {
    let payload = json!({
        "session_id": session,
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_input": {"file_path": cwd.join(file), "content": "x"},
        "cwd": cwd,
    });
    hook(test_box, cwd, "tool.before", &payload)
}

/// The session's own stop, after the product already continued it once (not refused, R7).
fn stops(test_box: &TestBox, session: &str, cwd: &Path) {
    let stop = json!({
        "session_id": session,
        "hook_event_name": "Stop",
        "stop_hook_active": true,
        "cwd": cwd,
    });
    assert_eq!(hook(test_box, cwd, "turn.end", &stop).0, Some(0));
}

/// The locks the remote holds now: lock refs whose tip is not released (a released lock keeps
/// its ref, so the ref's history names who held it).
fn locks(test_box: &TestBox, remote: &Path) -> Vec<String> {
    let listed = test_box.git_bare(
        remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/locks/"],
    );
    listed
        .lines()
        .filter(|name| {
            let lock = test_box.git_bare(remote, &["show", &format!("{name}:lock.json")]);
            let lock: serde_json::Value = serde_json::from_str(&lock).unwrap();
            lock["released"] != true
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_second_machine_is_refused_until_the_holder_stops() {
    let (test_box, first, second, remote) = two_machines(30);
    let (code, err) = edit(&test_box, "s-first", &first, "art/logo.bin");
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(locks(&test_box, &remote).len(), 1, "one lock on the remote");

    let (code, err) = edit(&test_box, "s-second", &second, "art/logo.bin");
    assert_eq!(
        code,
        Some(2),
        "the second machine edited a locked path: {err}"
    );
    assert!(err.contains("refused PATH_HELD"), "{err}");
    assert!(
        err.contains("art/logo.bin") && err.contains("s-first"),
        "{err}"
    );
    assert_eq!(locks(&test_box, &remote).len(), 1, "still one lock");

    // The holder's own edits go through; a path no lock names is never judged.
    assert_eq!(
        edit(&test_box, "s-first", &first, "art/logo.bin").0,
        Some(0)
    );
    assert_eq!(edit(&test_box, "s-second", &second, "src/a.rs").0, Some(0));

    stops(&test_box, "s-first", &first);
    assert!(
        locks(&test_box, &remote).is_empty(),
        "the stop left its lock"
    );
    let (code, err) = edit(&test_box, "s-second", &second, "art/logo.bin");
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn a_lock_past_the_takeover_window_is_taken_over() {
    // lease_s = 1: a takeover window of 2 s.
    let (test_box, first, second, remote) = two_machines(1);
    assert_eq!(
        edit(&test_box, "s-first", &first, "art/logo.bin").0,
        Some(0)
    );
    let before = test_box.git_bare(
        &remote,
        &["for-each-ref", "--format=%(objectname)", "refs/bay/locks/"],
    );
    std::thread::sleep(std::time::Duration::from_secs(3));
    let (code, err) = edit(&test_box, "s-second", &second, "art/logo.bin");
    assert_eq!(code, Some(0), "an expired lock still held: {err}");
    let after = test_box.git_bare(
        &remote,
        &["for-each-ref", "--format=%(objectname)", "refs/bay/locks/"],
    );
    assert_ne!(before, after, "the lock was not taken over");
}

#[test]
fn offline_a_lockable_edit_is_refused() {
    let (test_box, _first, second, _remote) = two_machines(30);
    let gone = test_box.path().join("gone.git");
    test_box.git_ok(
        &second,
        &["remote", "set-url", "origin", gone.to_str().unwrap()],
    );
    let (code, err) = edit(&test_box, "s-second", &second, "art/logo.bin");
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("refused REMOTE_UNREACHABLE"), "{err}");
    // A path no lock names needs no remote.
    assert_eq!(edit(&test_box, "s-second", &second, "src/a.rs").0, Some(0));
}

#[test]
fn a_bay_host_takes_the_lock_refs() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let first = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&first, &host);
    test_box.seed_with_check(&first, &[("art/logo.bin", "logo\n")], &policy_files(30));
    // A host with a registry judges every bay ref it is pushed (§ 10a).
    let keys = test_box.keys();
    test_box.enroll(&first, &keys);
    // The lock is signed by the agent key enrolled for its actor (G361).
    test_box.use_keys(&first, &keys);
    let second = test_box.clone_of(&host, "second");

    let (code, err) = edit(&test_box, "s-first", &first, "art/logo.bin");
    assert_eq!(code, Some(0), "the host refused the lock: {err}");
    assert_eq!(locks(&test_box, &host).len(), 1);
    assert_eq!(
        edit(&test_box, "s-second", &second, "art/logo.bin").0,
        Some(2)
    );
    stops(&test_box, "s-first", &first);
    assert!(
        locks(&test_box, &host).is_empty(),
        "the host kept a released lock"
    );
}

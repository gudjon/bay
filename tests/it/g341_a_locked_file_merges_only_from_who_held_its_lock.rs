//! G341 (BACKLOG 57 phase 2, owner ruling 2026-10-22: "57 approved, go ahead"): a locked file
//! merges only from who held its lock, on the newest copy of it.
//!
//! A lock orders edits in time; two more rules keep a file that never merges whole:
//! - The successor rule: a lock is granted only when the work's base holds main's last change
//!   to the path. An edit from an older copy would drop that change, so it is refused
//!   `INPUT_CHANGED`, naming main's commit; with the newest main the edit goes through.
//! - The merge guard: a change to a lockable path merges only when some commit of the path's
//!   lock names the change (a lock ref keeps its history; a release is a commit, not a delete).
//!   An edit made around the lock, with no hook, is refused `LOCK_NOT_HELD` at candidate build.
//!   `bay save` takes the locks of the lockable files it saves, so a person's save passes it.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/locks.rs` `successor`: return `Ok(())` at once (the stale edit goes through).
//! - `src/publish.rs` `lock_not_held`: return `Ok(None)` at once (the edit around the lock
//!   merges).
//! - `src/save.rs` `save_in`: drop the `crate::locks::take` call (a person's save is refused
//!   `LOCK_NOT_HELD`).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};
use support::{TestBox, UNCHECKED_POLICY, result_json};

/// A repository whose `art/` is lockable, and a second clone of its remote. Returns the box,
/// both clones and the remote.
fn two_clones() -> (TestBox, PathBuf, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let first = test_box.repo_any("repo");
    test_box.seed_with_check(
        &first,
        &[("art/logo.bin", "logo 1\n"), ("src/a.rs", "fn a() {}\n")],
        &[
            (
                ".bay/policy.toml".to_owned(),
                format!("{UNCHECKED_POLICY}\n[locks]\npaths = [\"art/\"]\n"),
            ),
            (
                ".bay/config.toml".to_owned(),
                "[runner]\nlease_s = 30\nheartbeat_s = 1\n".to_owned(),
            ),
        ],
    );
    let remote = test_box.path().join("repo-remote.git");
    let second = test_box.clone_of(&remote, "second");
    (test_box, first, second, remote)
}

/// `tool.before` of `session` writing `file` (relative to `cwd`); returns (exit, stderr).
fn edit(test_box: &TestBox, session: &str, cwd: &Path, file: &str) -> (Option<i32>, String) {
    let payload = json!({
        "session_id": session,
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_input": {"file_path": cwd.join(file), "content": "x"},
        "cwd": cwd,
    });
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", "tool.before"])
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

/// A commit on the remote's main that changes `art/logo.bin`, pushed from `clone` with plain
/// Git (no bay, no lock).
fn main_changes_the_logo(test_box: &TestBox, clone: &Path) {
    std::fs::write(clone.join("art/logo.bin"), "logo 2\n").unwrap();
    test_box.git_ok(clone, &["commit", "-q", "-am", "logo 2"]);
    test_box.git_ok(clone, &["push", "-q", "origin", "main"]);
}

#[test]
fn an_edit_from_an_older_copy_is_refused_until_main_is_brought_in() {
    let (test_box, first, second, _remote) = two_clones();
    main_changes_the_logo(&test_box, &first);
    let (code, err) = edit(&test_box, "s-second", &second, "art/logo.bin");
    assert_eq!(
        code,
        Some(2),
        "an edit of an older copy went through: {err}"
    );
    assert!(err.contains("refused INPUT_CHANGED"), "{err}");
    test_box.git_ok(&second, &["pull", "-q", "--ff-only"]);
    let (code, err) = edit(&test_box, "s-second", &second, "art/logo.bin");
    assert_eq!(code, Some(0), "{err}");
}

#[test]
fn an_edit_around_the_lock_is_refused_at_merge_and_one_under_it_merges() {
    let (test_box, first, _second, remote) = two_clones();
    // Around the lock: the file written with no hook.
    let started = test_box.bay_ok(&first, &["start", "a new logo", "--paths", "art/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("art/logo.bin"), "logo 3\n").unwrap();
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "LOCK_NOT_HELD", "{refused}");
    assert_eq!(
        refused["detail"]["paths"],
        json!(["art/logo.bin"]),
        "{refused}"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:art/logo.bin"]),
        "logo 1"
    );

    // Under the lock: the hook takes it for this change, then the same edit merges.
    let started = test_box.bay_ok(&first, &["start", "a newer logo", "--paths", "art/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let (code, err) = edit(&test_box, "s-first", &view, "art/logo.bin");
    assert_eq!(code, Some(0), "{err}");
    std::fs::write(view.join("art/logo.bin"), "logo 4\n").unwrap();
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:art/logo.bin"]),
        "logo 4"
    );
}

#[test]
fn a_persons_save_takes_the_lock_and_a_save_of_an_older_copy_is_refused() {
    let (test_box, first, second, remote) = two_clones();
    std::fs::write(first.join("art/logo.bin"), "logo 5\n").unwrap();
    let (code, saved) = result_json(test_box.bay(&first).args(["save", "a new logo"]));
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:art/logo.bin"]),
        "logo 5"
    );

    // The second clone's copy is older than main's: its save stops before anything merges.
    std::fs::write(second.join("art/logo.bin"), "logo 6\n").unwrap();
    let (code, refused): (i32, Value) =
        result_json(test_box.bay(&second).args(["save", "another logo"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "INPUT_CHANGED", "{refused}");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:art/logo.bin"]),
        "logo 5"
    );
}

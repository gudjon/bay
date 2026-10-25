//! G341 (BACKLOG 57 phase 2, owner ruling 2026-10-22: "57 approved, go ahead"): a locked file
//! merges only from who held its lock, on the newest copy of it.
//!
//! A lock orders edits in time; two more rules keep a file that never merges whole:
//! - The successor rule: a lock is granted only when the work's base holds main's last change
//!   to the path. An edit from an older copy would drop that change, so it is refused
//!   `INPUT_CHANGED`, naming main's commit; with the newest main the edit goes through.
//! - The merge guard: a change to a lockable path merges only when the lock's current holder
//!   named the change (a release is a commit that keeps its holder; a lock another session
//!   took since counts for nothing).
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
            (".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned()),
            (".gitattributes".to_owned(), "art/** lockable\n".to_owned()),
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

/// The turn end of `session` in `cwd`: its locks are released.
fn stops(test_box: &TestBox, session: &str, cwd: &Path) {
    let stop = json!({
        "session_id": session,
        "hook_event_name": "Stop",
        "stop_hook_active": true,
        "cwd": cwd,
    });
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", "turn.end"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stop.to_string().as_bytes())
        .unwrap();
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(0));
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

/// The Mac's review of 0.4.0 (finding A): another session holds the lock now, and the same
/// machine user once held it too. A hookless change of the file from another clone is still
/// refused: the guard counts only a lock commit that names the change it judges, never the
/// actor, which every agent and person on a machine shares.
///
/// Guard to mutate: `src/locks.rs` `held_by`: also take a lock commit with no change whose
/// actor is the merging actor (the hookless change merges).
#[test]
fn a_lock_held_by_another_session_of_the_same_user_still_refuses_a_hookless_change() {
    let (test_box, first, second, remote) = two_clones();
    // Session s-a, in the first clone's main checkout, holds the lock now (it names no change).
    let (code, err) = edit(&test_box, "s-a", &first, "art/logo.bin");
    assert_eq!(code, Some(0), "{err}");
    // The second clone, same machine user, changes the file in a view with no hook.
    let started = test_box.bay_ok(&second, &["start", "a sneaky logo", "--paths", "art/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("art/logo.bin"), "logo 7\n").unwrap();
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "LOCK_NOT_HELD", "{refused}");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:art/logo.bin"]),
        "logo 1"
    );
}

/// The owner, on the Mac's review of 0.4.0: only the lock's current holder counts. A change
/// whose session released the lock at its turn's end still merges while no one has taken it
/// since; once another session takes it, the change's earlier lock counts for nothing, and its
/// hookless edit is refused until its session takes the lock again.
///
/// Guards to mutate:
/// - `src/locks.rs` `held_by`: drop the `take_while` (any past lock commit naming the change
///   counts, and the edit merges while another session holds the lock).
/// - `src/publish.rs` `retry_blocked`: drop the `LockNotHeld` exception (the retry after the
///   lock is taken again returns the old refusal, unchanged).
#[test]
fn only_the_locks_current_holder_counts_never_a_lock_taken_over_since() {
    let (test_box, first, second, remote) = two_clones();
    // Released at the turn's end, taken by no one since: the change merges.
    let started = test_box.bay_ok(&first, &["start", "a released logo", "--paths", "art/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let (code, err) = edit(&test_box, "s-c", &view, "art/logo.bin");
    assert_eq!(code, Some(0), "{err}");
    std::fs::write(view.join("art/logo.bin"), "logo 8\n").unwrap();
    stops(&test_box, "s-c", &view);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");

    // Released, then taken by another session: the change's earlier lock counts for nothing.
    let started = test_box.bay_ok(&first, &["start", "a taken logo", "--paths", "art/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let (code, err) = edit(&test_box, "s-c", &view, "art/logo.bin");
    assert_eq!(code, Some(0), "{err}");
    std::fs::write(view.join("art/logo.bin"), "logo 9\n").unwrap();
    stops(&test_box, "s-c", &view);
    test_box.git_ok(&second, &["pull", "-q", "--ff-only"]);
    let (code, err) = edit(&test_box, "s-d", &second, "art/logo.bin");
    assert_eq!(code, Some(0), "{err}");
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "LOCK_NOT_HELD", "{refused}");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:art/logo.bin"]),
        "logo 8"
    );

    // Once s-d's turn ends, the change's session takes the lock again, and the edit merges.
    stops(&test_box, "s-d", &second);
    let (code, err) = edit(&test_box, "s-c", &view, "art/logo.bin");
    assert_eq!(code, Some(0), "{err}");
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:art/logo.bin"]),
        "logo 9"
    );
}

/// The Mac's two edges for the guard. In a view, a hooked agent's lock names the very change the
/// merge judges. In the main checkout, the agent's own `bay save` re-takes the lock its hook
/// took, as the same session (`BAY_SESSION`, which Claude Code hands the agent's shell), and is
/// never refused `PATH_HELD` by it.
///
/// Guard to mutate: `src/locks.rs` `mine`: return `false` (the save is refused `PATH_HELD` by
/// its own agent's lock).
#[test]
fn an_agents_own_lock_names_its_change_and_its_own_save_retakes_it() {
    let (test_box, first, second, remote) = two_clones();
    let started = test_box.bay_ok(&first, &["start", "a held logo", "--paths", "art/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let change = started["change"].as_str().unwrap().to_owned();
    let (code, err) = edit(&test_box, "s-a", &view, "art/logo.bin");
    assert_eq!(code, Some(0), "{err}");
    let tip = test_box.git_bare(
        &remote,
        &["for-each-ref", "--format=%(objectname)", "refs/bay/locks/"],
    );
    let lock: Value =
        serde_json::from_str(&test_box.git_bare(&remote, &["show", &format!("{tip}:lock.json")]))
            .unwrap();
    assert_eq!(lock["change"], json!(change), "{lock}");
    std::fs::write(view.join("art/logo.bin"), "logo 10\n").unwrap();
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["change"], json!(change), "{merged}");
    stops(&test_box, "s-a", &view);

    // The main checkout: session s-b's hook takes the lock, then s-b's own save re-takes it.
    test_box.git_ok(&second, &["pull", "-q", "--ff-only"]);
    let (code, err) = edit(&test_box, "s-b", &second, "art/logo.bin");
    assert_eq!(code, Some(0), "{err}");
    std::fs::write(second.join("art/logo.bin"), "logo 11\n").unwrap();
    let (code, saved) = result_json(
        test_box
            .bay(&second)
            .env("BAY_SESSION", "s-b")
            .args(["save", "another held logo"]),
    );
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:art/logo.bin"]),
        "logo 11"
    );
}

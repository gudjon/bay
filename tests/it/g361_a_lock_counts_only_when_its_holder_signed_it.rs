//! G361 (BACKLOG 57, owner ruling 2026-10-22: "a signed, create-only hold ref"): a lock counts
//! only when its holder signed it.
//!
//! Under a key registry that signs every record, the lock commit an edit takes is signed by the
//! machine's agent key, and only a lock commit signed by the agent key enrolled for the actor it
//! names counts:
//! - a signed lock lets its change merge;
//! - an unsigned lock naming a change, written straight into a remote's ref as a plain remote
//!   would hold it, holds nothing at merge (`LOCK_NOT_HELD`);
//! - an unsigned lock is one bay cannot read, so an edit takes it over instead of being refused
//!   `PATH_HELD`;
//! - a bay host refuses an unsigned lock outright.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/locks.rs` `signed_lock`: return the lock before the signature check (the forged lock
//!   holds, so the hookless change merges and the edit is refused `PATH_HELD`).
//! - `src/locks.rs` `lock_commit`: always `--no-gpg-sign` (the host refuses the signed edit's
//!   lock, so the edit is refused).
//! - `src/hook.rs` `Judge::lock`: return `Ok(())` at once (the host takes the forged lock).

// Each case runs shell scripts (a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use support::talk::{TalkCase, hosted_talk_seeded};
use support::{UNCHECKED_POLICY, result_json, unchecked_files};

/// The talk host with `art/` lockable under a registry that signs every record. `agent`, on
/// clone `a`, also holds the runner and producer keys, so its changes merge.
fn case() -> TalkCase {
    hosted_talk_seeded(|test_box, a| {
        let mut files = unchecked_files();
        files.retain(|(path, _)| path != ".bay/policy.toml");
        files.push((".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned()));
        files.push((".gitattributes".to_owned(), "art/** lockable\n".to_owned()));
        test_box.seed_with_check(
            a,
            &[("art/logo.bin", "logo 1\n"), ("src/a.rs", "fn a() {}\n")],
            &files,
        );
    })
}

/// `tool.before` of `actor`'s session `session` writing `file` (relative to `cwd`); returns
/// (exit, stderr).
fn edit(
    case: &TalkCase,
    actor: &str,
    session: &str,
    cwd: &Path,
    file: &str,
) -> (Option<i32>, String) {
    let payload = json!({
        "session_id": session,
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_input": {"file_path": cwd.join(file), "content": "x"},
        "cwd": cwd,
    });
    let mut child = case
        .test_box
        .bay_process(cwd)
        .env("BAY_ACTOR", actor)
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

/// `agent` starts a change of `art/` on clone `a`: its id and view.
fn start(case: &TalkCase, intent: &str) -> (String, PathBuf) {
    let (code, started) = result_json(
        case.bay_as("agent")
            .args(["start", intent, "--paths", "art/"]),
    );
    assert_eq!(code, 0, "{started}");
    (
        started["change"].as_str().unwrap().to_owned(),
        PathBuf::from(started["detail"]["view"].as_str().unwrap()),
    )
}

/// `bay merge --wait` as `agent` in `view`.
fn merge(case: &TalkCase, view: &Path) -> (i32, Value) {
    let mut cmd = case.test_box.bay(view);
    cmd.env("BAY_ACTOR", "agent").env("BAY_SESSION", "s-agent");
    result_json(cmd.args(["merge", "--wait"]))
}

/// An unsigned `bay.lock/1` commit of `art/logo.bin` naming `actor`, `session` and `change`,
/// made in the bare repository `bare`.
fn forged_lock(bare: &Path, actor: &str, session: &str, change: Option<&str>) -> String {
    let lock = json!({
        "schema": "bay.lock/1",
        "path": "art/logo.bin",
        "actor": actor,
        "session": session,
        "change": change,
        "at_s": bay::live::unix_now(),
        "released": false,
    });
    let blob = stdin_git(
        bare,
        &["hash-object", "-w", "--stdin"],
        &format!("{lock:#}\n"),
    );
    let tree = stdin_git(
        bare,
        &["mktree"],
        &format!("100644 blob {blob}\tlock.json\n"),
    );
    stdin_git(
        bare,
        &[
            "commit-tree",
            "--no-gpg-sign",
            &tree,
            "-m",
            "lock art/logo.bin",
        ],
        "",
    )
}

fn logo_ref() -> String {
    bay::locks::lock_ref("art/logo.bin")
}

#[test]
fn a_signed_lock_lets_its_change_merge() {
    let case = case();
    let (_, view) = start(&case, "a new logo");
    let (code, err) = edit(&case, "agent", "s-agent", &view, "art/logo.bin");
    assert_eq!(code, Some(0), "the signed lock was not taken: {err}");
    let tip = case.host_tip(&logo_ref()).expect("no lock on the host");
    assert!(
        case.test_box
            .git_bare(&case.host, &["cat-file", "commit", &tip])
            .contains("gpgsig"),
        "the lock commit is not signed"
    );
    std::fs::write(view.join("art/logo.bin"), "logo 2\n").unwrap();
    let (code, merged) = merge(&case, &view);
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    assert_eq!(
        case.test_box
            .git_bare(&case.host, &["show", "main:art/logo.bin"]),
        "logo 2"
    );
}

#[test]
fn a_lock_nobody_signed_holds_nothing_at_merge() {
    let case = case();
    let (change, view) = start(&case, "a sneaky logo");
    // The file written with no hook, and a lock naming the change that no key signed, written
    // straight into the remote's ref past its hook, as a plain remote would hold it.
    std::fs::write(view.join("art/logo.bin"), "logo 3\n").unwrap();
    let forged = forged_lock(&case.host, "agent", "s-agent", Some(&change));
    case.test_box
        .git_bare(&case.host, &["update-ref", &logo_ref(), &forged]);
    let (code, refused) = merge(&case, &view);
    assert_eq!(code, 10, "a forged lock let the change merge: {refused}");
    assert_eq!(refused["reason"], "LOCK_NOT_HELD", "{refused}");
    assert_eq!(
        case.test_box
            .git_bare(&case.host, &["show", "main:art/logo.bin"]),
        "logo 1"
    );
}

#[test]
fn an_edit_takes_over_a_lock_nobody_signed() {
    let case = case();
    // A lock under agent-b's name that no key signed, written past the host's hook.
    let forged = forged_lock(&case.host, "agent-b", "s-agent-b", None);
    case.test_box
        .git_bare(&case.host, &["update-ref", &logo_ref(), &forged]);
    let (_, view) = start(&case, "a newer logo");
    let (code, err) = edit(&case, "agent", "s-agent", &view, "art/logo.bin");
    assert_eq!(code, Some(0), "a forged lock refused the edit: {err}");
    let tip = case.host_tip(&logo_ref()).unwrap();
    assert_ne!(tip, forged, "the forged lock still stands");
}

#[test]
fn the_host_refuses_a_lock_nobody_signed() {
    let case = case();
    let git_dir = case.b.join(".git");
    let forged = forged_lock(&git_dir, "agent-b", "s-agent-b", None);
    let out = Command::new("git")
        .current_dir(&case.b)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["push", "-q", "origin", &format!("{forged}:{}", logo_ref())])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "the host took a forged lock: {stderr}"
    );
    assert!(
        stderr.contains("is not a bay.lock/1 commit signed by the agent key"),
        "{stderr}"
    );
    assert_eq!(case.host_tip(&logo_ref()), None);
}

/// `git --git-dir=<bare> <args>` with `input` on stdin: its trimmed stdout.
fn stdin_git(bare: &Path, args: &[&str], input: &str) -> String {
    let mut child = Command::new("git")
        .arg(format!("--git-dir={}", bare.display()))
        .args(args)
        .env("GIT_AUTHOR_NAME", "agent")
        .env("GIT_AUTHOR_EMAIL", "agent@bay.invalid")
        .env("GIT_COMMITTER_NAME", "agent")
        .env("GIT_COMMITTER_EMAIL", "agent@bay.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

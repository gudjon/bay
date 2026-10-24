//! G346 (BACKLOG 52 H4, owner ruling 2026-10-22; Grok fixtures recorded 2026-10-24): a guest
//! product's tool calls are judged.
//!
//! Grok Build runs a repository's Claude Code hook entries with payloads of its own
//! (`tests/fixtures/grok-1.0.52`): Claude Code's keys beside its own, and its own tool names.
//! bay read none of them, so through Grok a force push, a stale write or a locked file went by
//! unjudged. A payload in Grok's shape is now judged as Claude Code's: its shell
//! (`run_terminal_command`) by the deny list, its `write` by the claims and the locks.
//!
//! Guard to mutate: `src/agent_hook.rs` `dispatch`: drop the `guest::as_claude` translation (the
//! force push and the locked write go through).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::{TestBox, UNCHECKED_POLICY};

/// A recorded Grok payload, moved into `repo`, with `edit` applied.
fn fixture(name: &str, repo: &Path, edit: impl FnOnce(&mut Value)) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/grok-1.0.52")
        .join(name);
    let mut payload: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    for key in ["cwd", "workspaceRoot"] {
        payload[key] = json!(repo);
    }
    edit(&mut payload);
    payload
}

/// `bay hook tool.before` of `payload` from `repo`: (exit, stderr).
fn tool_before(test_box: &TestBox, repo: &Path, payload: &Value) -> (Option<i32>, String) {
    let mut child = test_box
        .bay_process(repo)
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

fn shell(command: &str) -> impl FnOnce(&mut Value) {
    let command = command.to_owned();
    move |payload| {
        payload["tool_input"]["command"] = json!(command);
        payload["toolInput"]["command"] = json!(command);
    }
}

#[test]
fn a_grok_shell_call_meets_the_deny_list() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &support::unchecked_files(),
    );
    let push = fixture(
        "PreToolUse-0.json",
        &repo,
        shell("git push --force origin main"),
    );
    let (code, err) = tool_before(&test_box, &repo, &push);
    assert_eq!(code, Some(2), "Grok's force push went by: {err}");
    assert!(err.contains("refused SHELL_DENIED"), "{err}");
    let echo = fixture("PreToolUse-0.json", &repo, shell("echo hello > a.txt"));
    assert_eq!(tool_before(&test_box, &repo, &echo).0, Some(0));
}

#[test]
fn a_grok_write_takes_a_lock_as_claude_codes_does() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("art/logo.bin", "logo\n")],
        &[(
            ".bay/policy.toml".to_owned(),
            format!("{UNCHECKED_POLICY}\n[locks]\npaths = [\"art/\"]\n"),
        )],
    );
    let target = repo.join("art/logo.bin");
    let write = fixture("PreToolUse-write-tool.json", &repo, |payload| {
        payload["tool_input"]["file_path"] = json!(target);
        payload["toolInput"]["file_path"] = json!(target);
    });
    let (code, err) = tool_before(&test_box, &repo, &write);
    assert_eq!(code, Some(0), "{err}");
    let locks = test_box.git_bare(
        &remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/locks/"],
    );
    assert_eq!(
        locks.lines().count(),
        1,
        "Grok's write took no lock: {locks:?}"
    );
}

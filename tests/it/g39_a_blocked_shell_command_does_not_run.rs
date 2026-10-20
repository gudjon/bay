//! G39. A blocked shell command does not run.
//!
//! What this file tests is bay's half: `bay hook pre-tool` receives each shape on the deny list
//! (`DESIGN` § 11) and returns the product's deny exit (exit 2 for Claude Code, the exit of a
//! stale write), also when the shape is wrapped in `bash -c`, `sh -c`, `env`, `xargs`, `sudo`,
//! `command` or `exec`, or in a shell wrapper inside another. It returns the allow exit for a
//! push of `refs/bay/changes/<id>` and for ordinary commands, wrapped or not. That the product
//! then does not run a denied command is the product's half, tested per product by G61.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::process::Stdio;

use support::TestBox;

fn pre_tool(test_box: &TestBox, repo: &std::path::Path, command: &str) -> (Option<i32>, String) {
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": repo,
    });
    let mut child = test_box
        .bay_process(repo)
        .args(["hook", "pre-tool"])
        .stdin(Stdio::piped())
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

#[test]
fn every_shape_on_the_deny_list_gets_the_deny_exit() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    for command in [
        "git reset --hard",
        "git clean -f",
        "git clean -fd",
        "git branch -D topic",
        "git checkout .",
        "git restore .",
        "git push --force origin topic",
        "git push --force-with-lease origin topic",
        "git push origin main",
        "git push",
    ] {
        let (code, reason) = pre_tool(&test_box, &repo, command);
        assert_eq!(code, Some(2), "{command} was allowed");
        assert!(reason.contains("bay denies"), "{command}: {reason}");
    }
}

#[test]
fn a_bay_ref_push_ordinary_commands_and_other_tools_get_the_allow_exit() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    for command in [
        "git push origin refs/bay/changes/abcd2345",
        "git status",
        "git checkout -b topic",
    ] {
        let (code, reason) = pre_tool(&test_box, &repo, command);
        assert_eq!(code, Some(0), "{command}: {reason}");
    }
    // Other tools and other events are not judged by the deny list.
    let mut child = test_box
        .bay_process(&repo)
        .args(["hook", "pre-tool"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"tool_name":"Edit","tool_input":{"file_path":"x"}}"#)
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
}

#[test]
fn a_denied_shape_inside_a_wrapper_gets_the_deny_exit() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    for command in [
        "bash -c 'git reset --hard'",
        "sh -c \"git push --force origin topic\"",
        "/bin/bash -lc 'git clean -fd'",
        "bash --norc -c 'cd sub && git push origin main'",
        "env git push origin main",
        "env GIT_TRACE=1 FOO=bar git reset --hard",
        "echo topic | xargs git branch -D",
        "xargs -I{} git push --force origin {} < branches.txt",
        "sudo git reset --hard",
        "command git restore .",
        "exec git checkout .",
        "bash -c \"env git push origin main\"",
        "env bash -c 'git push --all origin'",
        "xargs sh -c 'git clean -f'",
        "sudo -u builder sh -c 'git push -f'",
        "bash -c \"sh -c 'git reset --hard'\"",
        "bash -c 'git status; git reset --hard'",
    ] {
        let (code, reason) = pre_tool(&test_box, &repo, command);
        assert_eq!(code, Some(2), "{command} was allowed");
        assert!(reason.contains("bay denies"), "{command}: {reason}");
    }
}

#[test]
fn an_ordinary_command_inside_a_wrapper_gets_the_allow_exit() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    for command in [
        "bash -c 'git status'",
        "sh -c \"git push origin refs/bay/changes/abcd2345\"",
        "env FOO=1 git push origin topic",
        "xargs git add",
        "bash -c \"env git log --oneline\"",
        "sudo -n true",
        "bash -c 'echo done'",
    ] {
        let (code, reason) = pre_tool(&test_box, &repo, command);
        assert_eq!(code, Some(0), "{command}: {reason}");
    }
}

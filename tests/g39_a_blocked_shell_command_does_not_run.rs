//! G39. A blocked shell command does not run.
//!
//! `bay hook pre-tool` receives each shape on the deny list (`DESIGN` § 11) and denies
//! it, so the agent product does not run it; the hook returns the deny exit of a stale write
//! (exit 2 for Claude Code). A normal push of `refs/bay/changes/<id>` still runs.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

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
fn a_normal_push_of_a_bay_ref_still_runs() {
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

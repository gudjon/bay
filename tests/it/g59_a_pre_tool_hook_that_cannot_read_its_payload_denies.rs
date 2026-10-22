//! G59. A pre-tool hook that cannot read its payload denies.
//!
//! Ruling R-H1 (AGENT-HOOKS-0.1 § 6.3, `context_invalid`, in bay's words): `bay hook tool.before`
//! is the one event bay judges before the act, so a payload it cannot read is a deny, never a
//! silent allow. A missing payload, one that is not JSON, not a JSON object, one with no string
//! `tool_name`, and a `Bash` payload with no string `tool_input.command` each exit 2 (the deny
//! exit of Claude Code's protocol) with one stderr line, and stdout holds a `bay.result/1`
//! naming `HOOK_INPUT_INVALID`. Every recorded `PreToolUse` payload keeps its old answer, and
//! the events that only record keep allowing an unreadable payload.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use support::TestBox;

/// `bay hook <event>` with `input` on stdin; returns (exit, stdout, stderr).
fn hook(
    test_box: &TestBox,
    cwd: &Path,
    event: &str,
    input: &[u8],
) -> (Option<i32>, String, String) {
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn an_unreadable_pre_tool_payload_gets_the_deny_exit_and_names_hook_input_invalid() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    for (what, input) in [
        ("a missing payload", &b""[..]),
        ("bytes that are not UTF-8", &b"\xff\xfe{"[..]),
        ("text that is not JSON", &b"not json"[..]),
        ("a JSON array", &b"[]"[..]),
        ("an empty object", &b"{}"[..]),
        (
            "a number as tool_name",
            &br#"{"hook_event_name":"PreToolUse","tool_name":7}"#[..],
        ),
        ("Bash with no tool_input", &br#"{"tool_name":"Bash"}"#[..]),
        (
            "Bash with a null command",
            &br#"{"tool_name":"Bash","tool_input":{"command":null}}"#[..],
        ),
        (
            "Bash with a list as command",
            &br#"{"tool_name":"Bash","tool_input":{"command":["git","push"]}}"#[..],
        ),
    ] {
        let (code, stdout, stderr) = hook(&test_box, &repo, "tool.before", input);
        assert_eq!(code, Some(2), "{what} was allowed: {stderr}");
        assert_eq!(stderr.lines().count(), 1, "{what}: {stderr}");
        assert!(stderr.contains("HOOK_INPUT_INVALID"), "{what}: {stderr}");
        let result: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("{what}: stdout {stdout:?}: {error}"));
        assert_eq!(result["schema"], "bay.result/1", "{what}: {result}");
        assert_eq!(result["decision"], "blocked", "{what}: {result}");
        assert_eq!(result["reason"], "HOOK_INPUT_INVALID", "{what}: {result}");
        assert!(result["allowed"].is_array(), "{what}: {result}");
        assert!(result["retry_when"].is_string(), "{what}: {result}");
    }
}

#[test]
fn every_recorded_pre_tool_payload_keeps_its_answer() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-code-2.1.302");
    for name in [
        "03-PreToolUse.json",
        "05-PreToolUse.json",
        "07-PreToolUse.json",
        "10-PreToolUse.json",
        "18-PreToolUse-denied.json",
    ] {
        let mut payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(fixtures.join(name)).unwrap()).unwrap();
        payload["session_id"] = "s-1".into();
        payload["cwd"] = repo.display().to_string().into();
        if let Some(file) = payload["tool_input"]["file_path"].as_str() {
            payload["tool_input"]["file_path"] =
                file.replace("<repo>", &repo.display().to_string()).into();
        }
        let (code, stdout, stderr) = hook(
            &test_box,
            &repo,
            "tool.before",
            payload.to_string().as_bytes(),
        );
        assert_eq!(code, Some(0), "{name}: {stdout} {stderr}");
        assert!(!stderr.contains("HOOK_INPUT_INVALID"), "{name}: {stderr}");
    }
    // A tool outside the deny list and the edit tools is still not judged.
    let (code, _, stderr) = hook(
        &test_box,
        &repo,
        "tool.before",
        br#"{"tool_name":"mcp__search","tool_input":{}}"#,
    );
    assert_eq!(code, Some(0), "{stderr}");
}

#[test]
fn an_event_that_only_records_still_allows_an_unreadable_payload() {
    // R-H1: only pre-tool judges before the act; the other events record, and a record that
    // cannot be written is a gap, never a deny.
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    for event in ["step.end", "turn.end", "turn.start", "session.end"] {
        for input in [&b""[..], &b"not json"[..], &b"[]"[..]] {
            let (code, _, stderr) = hook(&test_box, &repo, event, input);
            assert_eq!(code, Some(0), "{event}: {stderr}");
        }
    }
}

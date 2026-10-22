//! G60. A hook judgement is recorded as an AGENT-HOOKS-0.1 verdict.
//!
//! Ruling R-H2 (adapt): each judging `tool.before` row of the session work record (`bay.session/1`)
//! carries a `verdict` in AGENT-HOOKS-0.1's words, `decision` `allow` or `deny`, `reason` a bay
//! reason code (never a `host_error:*` one), and `warnings` for an allow with a notice.
//! `bay.result/1` keeps its own words. The widening of H3: a shell deny is recorded too, with the
//! deny-list shape's name and the wrapper it was found in, never the command text, which can
//! carry a secret (AGENT-HOOKS-0.1 § 10.3, a payload-free record).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};
use support::TestBox;

/// A string that appears in the commands below and nowhere in bay.
const MARKER: &str = "g60-secret-q7x2kz";

/// `bay hook tool.before` with `payload` on stdin; returns (exit, stdout).
fn pre_tool(test_box: &TestBox, cwd: &Path, payload: &Value) -> (Option<i32>, String) {
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
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn bash(session: &str, cwd: &Path, command: &str) -> Value {
    json!({
        "session_id": session,
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": cwd,
    })
}

fn sessions(repo: &Path) -> PathBuf {
    repo.join(".git/bay/sessions")
}

fn rows(repo: &Path, session: &str) -> Vec<Value> {
    let path = sessions(repo).join(format!("{session}.jsonl"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// An AGENT-HOOKS-0.1 verdict (§ 5) as bay writes one: only the verdict's own field names, a
/// decision bay speaks, and bay reason codes that are not the host's reserved `host_error:*`.
fn assert_verdict(row: &Value, decision: &str, reason: &str) {
    let verdict = &row["verdict"];
    let fields = verdict
        .as_object()
        .unwrap_or_else(|| panic!("no verdict: {row}"));
    for key in fields.keys() {
        assert!(
            [
                "decision",
                "reason",
                "message",
                "warnings",
                "approval",
                "transform",
                "evidence",
                "result_labels"
            ]
            .contains(&key.as_str()),
            "{key} is not an AGENT-HOOKS-0.1 verdict field: {row}"
        );
    }
    assert_eq!(verdict["decision"], decision, "{row}");
    assert_eq!(verdict["reason"], reason, "{row}");
    let codes = std::iter::once(&verdict["reason"]).chain(
        verdict["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|w| &w["reason"]),
    );
    for code in codes {
        let code = code.as_str().unwrap_or_else(|| panic!("{row}"));
        assert!(!code.starts_with("host_error:"), "{row}");
        assert!(
            bay::result::Reason::from_code(code).is_some(),
            "{code}: {row}"
        );
    }
}

/// Every file bay keeps for this repository, read as bytes: none holds `MARKER`.
fn assert_marker_absent(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            assert_marker_absent(&path);
        } else if let Ok(bytes) = std::fs::read(&path) {
            assert!(
                !String::from_utf8_lossy(&bytes).contains(MARKER),
                "{} holds the command text",
                path.display()
            );
        }
    }
}

#[test]
fn a_shell_deny_is_recorded_as_a_deny_with_its_shape_and_never_its_command() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let plain = format!("git reset --hard {MARKER}");
    let (code, stdout) = pre_tool(&test_box, &repo, &bash("s-1", &repo, &plain));
    assert_eq!(code, Some(2));
    assert_eq!(stdout, "", "a shell deny prints no result: {stdout}");
    let wrapped = format!("bash -c 'git push --force origin {MARKER}'");
    let (code, _) = pre_tool(&test_box, &repo, &bash("s-1", &repo, &wrapped));
    assert_eq!(code, Some(2));
    let nested = format!("sh -c \"env {MARKER}=1 git push --all origin\"");
    let (code, _) = pre_tool(&test_box, &repo, &bash("s-1", &repo, &nested));
    assert_eq!(code, Some(2));
    // An allowed command writes no row (the silent pre-tool path).
    let (code, _) = pre_tool(
        &test_box,
        &repo,
        &bash("s-1", &repo, &format!("echo {MARKER}")),
    );
    assert_eq!(code, Some(0));

    let recorded = rows(&repo, "s-1");
    assert_eq!(recorded.len(), 3, "{recorded:?}");
    for row in &recorded {
        assert_eq!(row["schema"], "bay.session/1", "{row}");
        assert_eq!(row["event"], "pre_tool_use", "{row}");
        assert_eq!(row["state"], "blocked(SHELL_DENIED)", "{row}");
        assert_verdict(row, "deny", "SHELL_DENIED");
        assert_eq!(row["refused"]["reason"], "SHELL_DENIED", "{row}");
    }
    assert_eq!(recorded[0]["refused"]["shape"], "git reset --hard");
    assert!(
        recorded[0]["refused"].get("wrapped").is_none(),
        "{}",
        recorded[0]
    );
    assert_eq!(recorded[1]["refused"]["shape"], "git push --force");
    assert_eq!(recorded[1]["refused"]["wrapped"], "bash -c");
    assert_eq!(recorded[2]["refused"]["shape"], "git push --all");
    assert_eq!(recorded[2]["refused"]["wrapped"], "sh -c");
    assert_marker_absent(&repo.join(".git"));
}

#[test]
fn an_unreadable_payload_is_recorded_as_a_deny_and_its_result_keeps_bay_words() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let payload = json!({
        "session_id": "s-2",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": null, "description": MARKER},
        "cwd": repo,
    });
    let (code, stdout) = pre_tool(&test_box, &repo, &payload);
    assert_eq!(code, Some(2));
    // bay.result/1 keeps its own decision words (INTERFACES § 1).
    let result: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(result["decision"], "blocked", "{result}");
    let recorded = rows(&repo, "s-2");
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0]["state"], "blocked(HOOK_INPUT_INVALID)");
    assert_verdict(&recorded[0], "deny", "HOOK_INPUT_INVALID");
    assert_marker_absent(&repo.join(".git"));
}

#[test]
fn a_write_allowed_with_a_notice_is_recorded_as_an_allow_with_a_warning() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    // An edit with no target and no coordination cache cannot be judged: R5's notice.
    let payload = json!({
        "session_id": "s-3",
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_input": {"content": MARKER},
        "cwd": repo,
    });
    let (code, stdout) = pre_tool(&test_box, &repo, &payload);
    assert_eq!(code, Some(0), "{stdout}");
    assert!(stdout.contains("COVERAGE_UNKNOWN"), "{stdout}");
    let recorded = rows(&repo, "s-3");
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_verdict(&recorded[0], "allow", "COVERAGE_UNKNOWN");
    assert_eq!(
        recorded[0]["verdict"]["warnings"],
        json!([{"reason": "COVERAGE_UNKNOWN"}]),
        "{}",
        recorded[0]
    );
    assert_marker_absent(&repo.join(".git"));
}

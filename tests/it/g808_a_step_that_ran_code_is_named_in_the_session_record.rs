//! G808 (Code Mode, 2026-11-20: an agent writes code that makes several tool calls in one step).
//! A step that ran code is named in the session's work record: its `step.end` row says
//! `from_code`, so a reader can tell the time of a step that ran code from a step of single calls.
//! The payloads are Claude Code's own, recorded in Code Mode
//! (`tests/fixtures/claude-code-2.1.329-code-mode`) and without it (`claude-code-2.1.302`).
//!
//! Guard to mutate, in `src/agent_hook.rs`: `from_code` answering false (the row names no code).

#![cfg(unix)]

use crate::support;

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use serde_json::Value;
use support::TestBox;

const SESSION: &str = "0d5bb0a4-5b5e-4b0c-9a4c-2f3f7a3c8e11";

/// The recorded payload `name` under `dir`, for this test's session in `cwd`.
fn fixture(dir: &str, name: &str, cwd: &Path) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
        .join(name);
    let mut payload: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    payload["session_id"] = Value::from(SESSION);
    payload["cwd"] = Value::from(cwd.display().to_string());
    payload
}

/// `bay hook step.end` with `payload` in `repo`.
fn step_end(test_box: &TestBox, repo: &Path, payload: &Value) {
    let mut child = test_box
        .bay_process(repo)
        .args(["hook", "step.end"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
}

/// The `step.end` rows of this test's session in `repo`'s work record.
fn step_rows(repo: &Path) -> Vec<Value> {
    std::fs::read_to_string(repo.join(format!(".git/bay/sessions/{SESSION}.jsonl")))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| row["event"] == "post_tool_batch")
        .collect()
}

#[test]
fn a_step_that_ran_code_is_named_and_a_step_of_single_calls_is_not() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, "main = \"main\"\n\n[agent]\nproduct = \"claude\"\n");
    // GIVEN a step of single calls, then a step that ran code
    step_end(
        &test_box,
        &repo,
        &fixture("claude-code-2.1.302", "08-PostToolBatch.json", &repo),
    );
    step_end(
        &test_box,
        &repo,
        &fixture(
            "claude-code-2.1.329-code-mode",
            "01-PostToolBatch-code.json",
            &repo,
        ),
    );
    // THEN the session's record names the second as run from code, and the first not
    let rows = step_rows(&repo);
    assert_eq!(rows.len(), 2, "{rows:#?}");
    assert!(rows[0].get("from_code").is_none(), "{rows:#?}");
    assert_eq!(rows[1]["from_code"], true, "{rows:#?}");
}

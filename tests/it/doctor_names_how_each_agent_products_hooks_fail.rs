//! H4. Doctor has a row per configured agent product that says whether its hooks fail closed,
//! that `Stop` cannot, and that the guarantee does not rest on the client hook.
//!
//! `DESIGN` § 15.2; the hook-standards analysis, item (e). The row is informational: it is red
//! only where bay's entries are missing, which the `hooks` row already reports.

// Each case runs shell scripts (a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::{TestBox, result_json, row};

const CLAUDE_ADAPTER: &str = "main = \"main\"\n\n[agent]\nproduct = \"claude\"\n";

fn claude_repo(test_box: &TestBox) -> std::path::PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    repo
}

fn agent_hooks_rows(result: &Value) -> Vec<&Value> {
    result["detail"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] == "agent-hooks")
        .collect()
}

#[test]
fn installed_claude_hooks_fail_closed_and_stop_cannot() {
    let test_box = TestBox::new();
    let repo = claude_repo(&test_box);
    test_box.bay_ok(&repo, &["install"]);

    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(agent_hooks_rows(&result).len(), 1, "{result}");
    let hooks = row(&result, "agent-hooks");
    assert_eq!(hooks["product"], "claude", "{hooks}");
    assert_eq!(hooks["state"], "green", "{hooks}");
    assert_eq!(hooks["early_refusal"], "fail_closed", "{hooks}");
    assert_eq!(hooks["pre_tool"]["on_failure"], "block", "{hooks}");
    assert_eq!(hooks["stop"]["fails_closed"], false, "{hooks}");
    assert_eq!(
        hooks["guarantee"], "merge and main hook (G8, G23)",
        "{hooks}"
    );
    let note = hooks["note"].as_str().unwrap();
    assert!(note.contains("merge and the host hook re-judge"), "{note}");
}

#[test]
fn a_pre_tool_entry_without_its_failure_setting_fails_open() {
    let test_box = TestBox::new();
    let repo = claude_repo(&test_box);
    test_box.bay_ok(&repo, &["install"]);
    let path = repo.join(".claude/settings.json");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut settings: Value = serde_json::from_str(&text).unwrap();
    for group in settings["hooks"]["PreToolUse"].as_array_mut().unwrap() {
        for hook in group["hooks"].as_array_mut().unwrap() {
            hook.as_object_mut().unwrap().remove("onFailure");
        }
    }
    std::fs::write(&path, settings.to_string()).unwrap();

    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "informational, never red on its own: {result}");
    let hooks = row(&result, "agent-hooks");
    assert_eq!(hooks["state"], "green", "{hooks}");
    assert_eq!(hooks["early_refusal"], "fail_open", "{hooks}");
    assert!(hooks["pre_tool"]["on_failure"].is_null(), "{hooks}");
}

#[test]
fn missing_entries_refuse_nothing_early_and_the_hooks_row_names_why() {
    let test_box = TestBox::new();
    let repo = claude_repo(&test_box);
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "HOOKS_MISSING");
    let hooks = row(&result, "agent-hooks");
    assert_eq!(hooks["state"], "red", "{hooks}");
    assert_eq!(hooks["early_refusal"], "none", "{hooks}");
    assert_eq!(
        hooks["guarantee"], "merge and main hook (G8, G23)",
        "{hooks}"
    );
}

#[test]
fn no_agent_adapter_has_no_agent_hooks_row() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert!(agent_hooks_rows(&result).is_empty(), "{result}");
}

//! G158. A hung hook denies within its timeout.
//!
//! The G61 probes (Claude Code 2.1.303): with `onFailure: "block"` a `PreToolUse` hook that times
//! out denies the tool call, and with no `timeout` in bay's entry the product's default holds
//! the agent for 600 s first. bay's hook takes about half a second, so `bay install` writes
//! `"timeout": 30` on bay's `PreToolUse` entry: a hung bay denies within 30 s. A re-install
//! puts it on an entry an earlier bay wrote without it, and doctor's `hooks` row names it.

// Each case runs shell scripts (a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::{TestBox, result_json, row};

const CLAUDE_ADAPTER: &str = "main = \"main\"\n\n[agent]\nproduct = \"claude\"\n";

fn settings(repo: &Path) -> Value {
    let text = std::fs::read_to_string(repo.join(".claude/settings.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// bay's `bay hook tool.before` hook in the settings.
fn pre_tool_hook(settings: &Value) -> Value {
    settings["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["hooks"].as_array().unwrap())
        .find(|hook| {
            hook["command"]
                .as_str()
                .is_some_and(|command| command.ends_with("bay hook tool.before"))
        })
        .cloned()
        .unwrap_or_else(|| panic!("no bay pre-tool hook: {settings}"))
}

fn claude_repo(test_box: &TestBox) -> std::path::PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    repo
}

#[test]
fn install_writes_a_thirty_second_timeout_on_the_pre_tool_entry() {
    let test_box = TestBox::new();
    let repo = claude_repo(&test_box);
    test_box.bay_ok(&repo, &["install"]);
    let hook = pre_tool_hook(&settings(&repo));
    assert_eq!(hook["timeout"], 30, "{hook}");
    assert_eq!(hook["onFailure"], "block", "{hook}");
    // Only the blocking entry carries it: the other events keep the product's default.
    let all = settings(&repo);
    let stop = &all["hooks"]["Stop"][0]["hooks"][0];
    assert!(stop.get("timeout").is_none(), "{stop}");

    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let hooks = row(&result, "hooks");
    assert_eq!(hooks["state"], "green", "{hooks}");
    assert_eq!(hooks["pre_tool_timeout_s"], 30, "{hooks}");
    assert!(hooks.get("timeout_note").is_none(), "{hooks}");
}

#[test]
fn a_reinstall_adds_the_timeout_to_an_entry_without_it() {
    let test_box = TestBox::new();
    let repo = claude_repo(&test_box);
    test_box.bay_ok(&repo, &["install"]);
    // The entry as an earlier bay wrote it: no timeout.
    let path = repo.join(".claude/settings.json");
    let mut old = settings(&repo);
    for group in old["hooks"]["PreToolUse"].as_array_mut().unwrap() {
        for hook in group["hooks"].as_array_mut().unwrap() {
            hook.as_object_mut().unwrap().remove("timeout");
        }
    }
    std::fs::write(&path, old.to_string()).unwrap();

    // Doctor accepts the entry (the guarantee does not rest on it) and says what is missing.
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let hooks = row(&result, "hooks");
    assert_eq!(hooks["state"], "green", "{hooks}");
    assert!(hooks["pre_tool_timeout_s"].is_null(), "{hooks}");
    let note = hooks["timeout_note"].as_str().unwrap_or_default();
    assert!(note.contains("run install again"), "{hooks}");

    test_box.bay_ok(&repo, &["install"]);
    let hook = pre_tool_hook(&settings(&repo));
    assert_eq!(hook["timeout"], 30, "{hook}");
}

//! G306. Each agent adapter is declared as data in the contract.
//!
//! `bay help --json` lists, in `detail.adapters`, one entry per agent product bay has an adapter
//! for. The Claude entry names every event `bay install` writes into `.claude/settings.json`,
//! each with the `onFailure`, `timeout` and `matcher` the written entry carries, and no event
//! bay does not install: the declaration and the settings come from one table.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/contract.rs`: `"adapters": adapters(),` (no adapters: no `claude` entry).
//! - `src/contract.rs`: `let events: Vec<Value> = CLAUDE_EVENTS` (a list other than the one
//!   install writes: an event missing or extra).
//! - `src/contract.rs`: `"on_failure": shape.on_failure.unwrap_or(DEFAULT_ON_FAILURE),`,
//!   `"timeout_s": shape.timeout_s,` and `"matcher": shape.matcher,` (a fixed value: the
//!   `PreToolUse` entry differs from the written one).
//! - `src/contract.rs` `adapters`: drop the Codex entry (the Codex case finds no adapter).
//! - `src/agent_settings.rs` `codex_group`: write its own timeout instead of
//!   `codex_entry_shape(event).timeout_s` (the declared timeout differs from the written one).
//! - `src/agent_settings.rs` `bay_group`: `hook["onFailure"] = json!(on_failure);`,
//!   `hook["timeout"] = json!(timeout_s);` and
//!   `group.insert("matcher".to_owned(), json!(matcher));` (the writer leaving the shape:
//!   the written entry differs from the declared one).

// `bay install` runs with a fake `claude` on PATH (a shell script), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::TestBox;

const CLAUDE_ADAPTER: &str = "main = \"main\"\n\n[agent]\nproduct = \"claude\"\n";

/// Every `(product_event, bay_event, hook, group)` bay installed: its own groups, whose hook
/// runs `bay hook <event>`.
fn installed(settings: &Value) -> Vec<(String, String, Value, Value)> {
    let mut out = Vec::new();
    for (event, groups) in settings["hooks"].as_object().unwrap() {
        for group in groups.as_array().unwrap() {
            for hook in group["hooks"].as_array().unwrap() {
                let Some(command) = hook["command"].as_str() else {
                    continue;
                };
                let Some((_, bay_event)) = command.rsplit_once("bay hook ") else {
                    continue;
                };
                out.push((
                    event.clone(),
                    bay_event.to_owned(),
                    hook.clone(),
                    group.clone(),
                ));
            }
        }
    }
    out
}

#[test]
fn the_contract_declares_each_event_install_writes_and_no_other() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    test_box.bay_ok(&repo, &["install"]);
    let text = std::fs::read_to_string(repo.join(".claude/settings.json")).unwrap();
    let settings: Value = serde_json::from_str(&text).unwrap();
    let written = installed(&settings);
    assert!(!written.is_empty(), "{settings}");

    let contract = test_box.bay_ok(test_box.path(), &["help"]);
    let adapters = contract["detail"]["adapters"]
        .as_array()
        .unwrap_or_else(|| panic!("no detail.adapters: {contract}"));
    let claude = adapters
        .iter()
        .find(|adapter| adapter["product"] == "claude")
        .unwrap_or_else(|| panic!("no claude adapter: {adapters:?}"));
    assert_eq!(claude["settings_file"], ".claude/settings.json", "{claude}");
    assert_eq!(claude["presence_test"], true, "{claude}");
    assert_eq!(claude["without_bay"], "allow", "{claude}");
    assert!(
        claude["floor"]
            .as_str()
            .is_some_and(|floor| !floor.is_empty()),
        "{claude}"
    );
    assert!(
        claude["cannot_see"]
            .as_array()
            .is_some_and(|list| !list.is_empty()),
        "{claude}"
    );
    let events = claude["events"].as_array().unwrap();

    for (product_event, bay_event, hook, group) in &written {
        let declared = events
            .iter()
            .find(|event| {
                event["product_event"] == *product_event && event["bay_event"] == *bay_event
            })
            .unwrap_or_else(|| panic!("{product_event} ({bay_event}) is not declared: {claude}"));
        let on_failure = hook["onFailure"].as_str().unwrap_or("allow");
        assert_eq!(
            declared["on_failure"], on_failure,
            "{product_event}: {declared} vs {hook}"
        );
        assert_eq!(
            declared["timeout_s"], hook["timeout"],
            "{product_event}: {declared} vs {hook}"
        );
        assert_eq!(
            declared["matcher"], group["matcher"],
            "{product_event}: {declared} vs {group}"
        );
        assert!(declared["blocks"].is_boolean(), "{declared}");
    }
    for event in events {
        assert!(
            written.iter().any(|(product_event, bay_event, _, _)| {
                event["product_event"] == *product_event && event["bay_event"] == *bay_event
            }),
            "declared but not installed: {event}"
        );
    }

    // Only pre-tool denies and stop refuses once (R7): those two block, the rest record.
    for event in events {
        let blocks = matches!(event["product_event"].as_str(), Some("PreToolUse" | "Stop"));
        assert_eq!(event["blocks"], blocks, "{event}");
    }
}

const CODEX_ADAPTER: &str = "main = \"main\"\n\n[agent]\nproduct = \"codex\"\n";

#[test]
fn the_codex_adapter_is_declared_as_install_writes_it_and_as_failing_open() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CODEX_ADAPTER);
    test_box.bay_ok(&repo, &["install"]);
    let text = std::fs::read_to_string(repo.join(".codex/hooks.json")).unwrap();
    let settings: Value = serde_json::from_str(&text).unwrap();
    let written = installed(&settings);
    assert!(!written.is_empty(), "{settings}");

    let contract = test_box.bay_ok(test_box.path(), &["help"]);
    let codex = contract["detail"]["adapters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|adapter| adapter["product"] == "codex")
        .unwrap_or_else(|| panic!("no codex adapter: {contract}"))
        .clone();
    assert_eq!(codex["settings_file"], ".codex/hooks.json", "{codex}");
    assert_eq!(codex["fails_open"], true, "{codex}");
    assert_eq!(codex["presence_test"], false, "{codex}");
    let events = codex["events"].as_array().unwrap();
    assert_eq!(events.len(), written.len(), "{codex} vs {settings}");
    for (product_event, bay_event, hook, _) in &written {
        let declared = events
            .iter()
            .find(|event| {
                event["product_event"] == *product_event && event["bay_event"] == *bay_event
            })
            .unwrap_or_else(|| panic!("{product_event} ({bay_event}) is not declared: {codex}"));
        assert_eq!(declared["on_failure"], "allow", "{declared}");
        assert_eq!(
            declared["timeout_s"], hook["timeout"],
            "{product_event}: {declared} vs {hook}"
        );
        assert_eq!(declared["matcher"], Value::Null, "{declared}");
    }
}

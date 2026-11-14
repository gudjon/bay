//! G716 (a fleet's doctor, 2026-11-14: every box read "not guarded", and a re-install would have
//! run one hook twice, because the project ran `talk-wait` only for some sessions). A project may
//! narrow bay's entry that never stops the agent: bay's presence test, the project's conditions,
//! then `exec bay hook <event>`. Doctor counts it as bay's, and `bay install` keeps it as the
//! project wrote it, with no second copy. A narrowed guard (`tool.before`) is not bay's.
//!
//! Guard to mutate: `src/agent_settings.rs` `narrows` (answer false: the row is red, and a
//! re-install adds a second `talk-wait`).

use crate::support;

use serde_json::Value;
use support::{TestBox, result_json, row};

const CLAUDE_ADAPTER: &str = "[agent]\nproduct = \"claude\"\n";

const CONDITION: &str = "if [ -n \"${SOME_HEADLESS_RUN:-}\" ]; then exit 0; fi";

fn narrow(settings: &mut Value, event: &str, bay_event: &str) {
    let plain = format!("command -v bay >/dev/null 2>&1 || exit 0; exec bay hook {bay_event}");
    let narrowed =
        format!("command -v bay >/dev/null 2>&1 || exit 0; {CONDITION}; exec bay hook {bay_event}");
    for group in settings["hooks"][event].as_array_mut().unwrap() {
        for hook in group["hooks"].as_array_mut().unwrap() {
            if hook["command"] == plain.as_str() {
                hook["command"] = Value::from(narrowed.clone());
            }
        }
    }
}

fn runs(settings: &Value, event: &str, bay_event: &str) -> Vec<String> {
    settings["hooks"][event]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["hooks"].as_array().unwrap().iter())
        .filter_map(|hook| hook["command"].as_str())
        .filter(|command| command.ends_with(&format!("exec bay hook {bay_event}")))
        .map(str::to_owned)
        .collect()
}

fn installed(test_box: &TestBox, repo: &std::path::Path) -> std::path::PathBuf {
    test_box.commit_config(repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    let (code, result) = result_json(test_box.bay(repo).arg("install"));
    assert_eq!(code, 0, "{result}");
    repo.join(".claude/settings.json")
}

fn read(path: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn a_narrowed_wait_entry_is_bays_and_a_reinstall_keeps_it() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let path = installed(&test_box, &repo);

    // GIVEN the project runs bay's wait only for some sessions
    let mut settings = read(&path);
    narrow(&mut settings, "Stop", "talk-wait");
    std::fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

    // THEN doctor counts the entry as bay's
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let hooks = row(&result, "hooks");
    assert_eq!(hooks["state"], "green", "{hooks}");

    // AND a re-install keeps it as the project wrote it, once
    let (code, again) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{again}");
    let waits = runs(&read(&path), "Stop", "talk-wait");
    assert_eq!(waits.len(), 1, "{waits:?}");
    assert!(waits[0].contains(CONDITION), "{waits:?}");
}

#[test]
fn a_narrowed_guard_is_not_bays() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let path = installed(&test_box, &repo);

    // GIVEN the project runs bay's tool guard only for some sessions
    let mut settings = read(&path);
    narrow(&mut settings, "PreToolUse", "tool.before");
    std::fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

    // THEN the guard is missing
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(row(&result, "hooks")["state"], "red", "{result}");
}

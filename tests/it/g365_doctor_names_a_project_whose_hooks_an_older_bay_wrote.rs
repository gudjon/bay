//! G365 (a fleet rollout's finding, 2026-10-25: a newer bay on a box showed a bare `HOOKS_MISSING`).
//! Doctor names a project whose hooks an older bay wrote.
//!
//! When the project's settings carry some of bay's hook entries and lack others, an older bay
//! wrote them and this one hooks more: doctor's hooks row names the bay events it lacks under
//! `behind`, and its note says the settings are older than this bay. The row is still red (the
//! entries are missing) and its recovery is the same. Settings with no bay entry at all are
//! plainly missing, with no `behind`.
//!
//! Guard to mutate: `src/agent_settings.rs` `claude_events_behind`: answer that no entry is
//! there (the row says only that hooks are missing).

use crate::support;

use support::{TestBox, result_json, row};

const CLAUDE_ADAPTER: &str = "[agent]\nproduct = \"claude\"\n";

#[test]
fn settings_an_older_bay_wrote_are_named_older_than_this_bay() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    let (code, installed) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{installed}");

    // An older bay wrote no question hook: drop bay's `tool.ask` group.
    let path = repo.join(".claude/settings.json");
    let mut settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    settings["hooks"]["PreToolUse"]
        .as_array_mut()
        .unwrap()
        .retain(|group| !group.to_string().contains("bay hook tool.ask"));
    std::fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "HOOKS_MISSING", "{result}");
    let hooks = row(&result, "hooks");
    assert_eq!(hooks["state"], "red", "{hooks}");
    assert_eq!(hooks["behind"], serde_json::json!(["tool.ask"]), "{hooks}");
    assert!(
        hooks["note"]
            .as_str()
            .is_some_and(|note| note.contains("written by an older bay")),
        "{hooks}"
    );
}

#[test]
fn settings_with_no_bay_entry_are_plainly_missing() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    let hooks = row(&result, "hooks");
    assert_eq!(hooks["state"], "red", "{hooks}");
    assert!(hooks.get("behind").is_none(), "{hooks}");
}

//! G324 (hook contract H1, owner ruling 2026-10-22): 0.3.0 renames bay's hook moments with no
//! aliases, so doctor names each entry an older bay wrote under an old name, and `bay install`
//! rewrites it.
//!
//! An entry such as `bay hook pre-tool` reaches bay under a name it no longer knows, and G323
//! refuses its tool calls. So the `hooks` row is red with the event missing, as before, and its
//! `old_names` names the product events whose bay entry still uses an old name, with the step:
//! run install again. After `bay install` no entry uses an old name.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/doctor.rs` `hooks_row`: drop the `old_names` field (doctor never names them).
//! - `src/agent_settings.rs` `renamed_claude_events`: `event != bay_event` to `false` (no
//!   entry is named).

// `bay install` and doctor run with a fake `claude` on PATH (a shell script), so Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::json;
use support::{TestBox, result_json, row};

const CLAUDE_ADAPTER: &str = "main = \"main\"\n\n[agent]\nproduct = \"claude\"\n";

/// Each 0.3.0 hook moment and the name a 0.2 bay wrote for it.
const RENAMED: &[(&str, &str)] = &[
    ("session.start", "session-start"),
    ("turn.start", "user-prompt-submit"),
    ("tool.before", "pre-tool"),
    ("step.end", "post-tool"),
    ("turn.end", "stop"),
    ("subagent.start", "subagent-start"),
    ("subagent.end", "subagent-stop"),
    ("session.wait", "notification"),
    ("compact.before", "pre-compact"),
    ("file.change", "file-changed"),
    ("session.end", "session-end"),
];

#[test]
fn entries_under_old_names_are_named_and_install_rewrites_them() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    test_box.bay_ok(&repo, &["install"]);
    let settings = repo.join(".claude/settings.json");

    // The settings file as a 0.2 bay wrote it: every entry under its old name.
    let mut text = std::fs::read_to_string(&settings).unwrap();
    for (new, old) in RENAMED {
        text = text.replace(&format!("bay hook {new}\""), &format!("bay hook {old}\""));
    }
    assert!(text.contains("bay hook pre-tool\""), "{text}");
    std::fs::write(&settings, &text).unwrap();

    let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));
    let hooks = row(&doctor, "hooks");
    assert_eq!(hooks["state"], "red", "{doctor}");
    let old = hooks["old_names"]
        .as_array()
        .unwrap_or_else(|| panic!("no old_names: {hooks}"));
    assert!(old.contains(&json!("PreToolUse")), "{hooks}");
    assert!(old.contains(&json!("Stop")), "{hooks}");
    assert!(
        hooks["old_names_note"]
            .as_str()
            .is_some_and(|note| note.contains("run install again")),
        "{hooks}"
    );

    // `bay install` rewrites each entry under its 0.3.0 name.
    test_box.bay_ok(&repo, &["install"]);
    let text = std::fs::read_to_string(&settings).unwrap();
    for (new, old) in RENAMED {
        assert!(
            !text.contains(&format!("bay hook {old}\"")),
            "{old}: {text}"
        );
        if *new != "session.wait" {
            assert!(text.contains(&format!("bay hook {new}\"")), "{new}: {text}");
        }
    }
    let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));
    let hooks = row(&doctor, "hooks");
    assert_eq!(hooks["state"], "green", "{doctor}");
    assert!(hooks.get("old_names").is_none(), "{hooks}");
}

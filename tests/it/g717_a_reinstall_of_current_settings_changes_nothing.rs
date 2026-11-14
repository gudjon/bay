//! G717 (a fleet's install, 2026-11-14: every re-install moved bay's stop entry behind the
//! project's narrowed one, so each box found the tracked settings file changed). `bay install`
//! rewrites each of bay's entries where it stands and adds only a missing one: a re-install of
//! settings that already carry bay's entries, in any order among the project's own, leaves the
//! file byte for byte as it was.
//!
//! Guard to mutate: `src/agent_settings.rs` `add_entries`: drop each of bay's groups and add it
//! again at the end (the file changes).

use crate::support;

use serde_json::Value;
use support::{TestBox, result_json};

const CLAUDE_ADAPTER: &str = "[agent]\nproduct = \"claude\"\n";

#[test]
fn a_reinstall_of_current_settings_changes_nothing() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    let (code, result) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{result}");
    let path = repo.join(".claude/settings.json");

    // GIVEN the project's own stop entry stands after bay's first one, and bay's wait is narrowed
    let mut settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let stop = settings["hooks"]["Stop"].as_array_mut().unwrap();
    stop.insert(
        1,
        serde_json::json!({"hooks": [{"type": "command", "command": "true"}]}),
    );
    for group in stop.iter_mut() {
        for hook in group["hooks"].as_array_mut().unwrap() {
            if hook["command"]
                == "command -v bay >/dev/null 2>&1 || exit 0; exec bay hook talk-wait"
            {
                hook["command"] = Value::from(
                    "command -v bay >/dev/null 2>&1 || exit 0; [ -n \"${SOME_RUN:-}\" ] && exit 0; exec bay hook talk-wait",
                );
            }
        }
    }
    let written = format!("{}\n", serde_json::to_string_pretty(&settings).unwrap());
    std::fs::write(&path, &written).unwrap();

    // WHEN bay installs again
    let (code, again) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{again}");

    // THEN the file is as it was
    assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
}

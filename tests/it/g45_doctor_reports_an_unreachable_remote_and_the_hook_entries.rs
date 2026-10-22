//! G45. Doctor reports an unreachable remote and the hook entries.
//!
//! `ROADMAP` § 1; `DESIGN` § 15.2–15.4.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, result_json, row};

const CLAUDE_ADAPTER: &str = "[agent]\nproduct = \"claude\"\n";

#[test]
fn an_unreachable_remote_is_reported_not_crashed() {
    let test_box = TestBox::new();
    let repo = test_box.repo_without_remote_any("repo");
    let missing = test_box.path().join("no-such-remote.git");
    test_box.git_ok(
        &repo,
        &["remote", "add", "origin", missing.to_str().unwrap()],
    );
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "REMOTE_UNREACHABLE");
    assert_eq!(row(&result, "remote")["state"], "not reachable");

    let line = test_box.bay(&repo).arg("doctor").output().unwrap();
    // The human status line is on stderr; stdout is the result document (INTERFACES § 8).
    let line = String::from_utf8(line.stderr).unwrap();
    assert!(
        line.contains("doctor not ready (remote not reachable)"),
        "{line}"
    );
}

#[test]
fn no_configured_remote_is_reported() {
    let test_box = TestBox::new();
    let repo = test_box.repo_without_remote_any("repo");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "REMOTE_UNREACHABLE");
    assert_eq!(row(&result, "remote")["state"], "not configured");
}

#[test]
fn missing_hook_entries_are_red_until_install_writes_them() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));

    // Another tool's entry, and user-global settings and global Git config bay must not touch.
    let settings_path = repo.join(".claude/settings.json");
    std::fs::create_dir_all(repo.join(".claude")).unwrap();
    let other = serde_json::json!({
        "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "other-tool check"}]}]},
        "permissions": {"allow": ["Bash(ls)"]}
    });
    let other_text = format!("{}\n", serde_json::to_string_pretty(&other).unwrap());
    std::fs::write(&settings_path, &other_text).unwrap();
    let other_group = serde_json::to_string_pretty(&other["hooks"]["PreToolUse"][0]).unwrap();
    let user_settings = test_box.home.join(".claude/settings.json");
    std::fs::create_dir_all(user_settings.parent().unwrap()).unwrap();
    std::fs::write(&user_settings, "{\"user\": true}\n").unwrap();
    std::fs::write(&test_box.global_config, "[user]\n\tname = someone\n").unwrap();

    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "HOOKS_MISSING");
    let hooks = row(&result, "hooks");
    assert_eq!(hooks["state"], "red");
    assert_eq!(hooks["recovery"], "run install again");
    // Doctor wrote nothing.
    assert_eq!(std::fs::read_to_string(&settings_path).unwrap(), other_text);

    let (code, installed) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{installed}");
    assert_eq!(installed["detail"]["unrelated_hooks_unchanged"], true);
    assert!(test_box.local_bin().join("bay").is_file());

    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let hooks = row(&result, "hooks");
    assert_eq!(hooks["state"], "green");
    assert_eq!(hooks["unrelated_hooks_unchanged"], true);

    let after = std::fs::read_to_string(&settings_path).unwrap();
    assert!(after.contains("bay hook tool.before"), "{after}");
    assert!(after.contains("bay hook session.start"), "{after}");
    // The other tool's group keeps its bytes, and stays first.
    let indented: String = other_group
        .lines()
        .map(|line| format!("      {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(after.contains(indented.trim_start()), "{after}");
    let other_at = after.find("other-tool check").unwrap();
    let bay_at = after.find("bay hook tool.before").unwrap();
    assert!(other_at < bay_at);
    assert!(after.contains("\"Bash(ls)\""));

    assert_eq!(
        std::fs::read_to_string(&user_settings).unwrap(),
        "{\"user\": true}\n"
    );
    assert_eq!(
        std::fs::read_to_string(&test_box.global_config).unwrap(),
        "[user]\n\tname = someone\n"
    );
}

#[test]
fn install_fails_and_names_the_recovery_when_its_directory_is_off_path() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let out = test_box
        .bay(&repo)
        .env(
            "PATH",
            format!("{}:/bin:/usr/bin", test_box.fakebin.display()),
        )
        .arg("install")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(20));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("is not on PATH. Put that directory on PATH and run install again"),
        "{text}"
    );
    assert!(!test_box.local_bin().join("bay").exists());
}

#[test]
fn a_second_install_replaces_the_same_file() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    for _ in 0..2 {
        let (code, result) = result_json(test_box.bay(&repo).arg("install"));
        assert_eq!(code, 0, "{result}");
    }
    let entries: Vec<_> = std::fs::read_dir(test_box.local_bin()).unwrap().collect();
    assert_eq!(entries.len(), 1);
}

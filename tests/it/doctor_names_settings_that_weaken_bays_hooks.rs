//! G154. Doctor names local settings that weaken bay's hooks.
//!
//! With the Claude adapter configured, bay's hook entries live in the project settings file
//! `bay install` writes. A settings file the agent product also reads can switch them off: the
//! repository's `.claude/settings.local.json`, or the user settings file in the product's config
//! directory (`CLAUDE_CONFIG_DIR`, else `$HOME/.claude`). One that sets `disableAllHooks: true`,
//! or gives one of bay's events hook entries without bay's own command, makes doctor's
//! `hook-overrides` row red, naming the file and the recovery. Doctor only reads.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestBox, result_json, row};

const CLAUDE_ADAPTER: &str = "[agent]\nproduct = \"claude\"\n";

/// A repository with the Claude adapter and bay's hooks installed: doctor is green.
fn installed(test_box: &TestBox) -> PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    let (code, result) = result_json(test_box.bay(&repo).arg("install"));
    assert_eq!(code, 0, "{result}");
    repo
}

fn write(path: &Path, value: &Value) -> String {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    std::fs::write(path, &text).unwrap();
    text
}

fn overrides(result: &Value) -> &Value {
    row(result, "hook-overrides")
}

#[test]
fn no_weakening_settings_is_green() {
    let test_box = TestBox::new();
    let repo = installed(&test_box);
    // A local file that adds another tool's hook and keeps bay's events alone weakens nothing.
    write(
        &repo.join(".claude/settings.local.json"),
        &json!({"hooks": {"Notification": [{"hooks": [{"type": "command", "command": "bay hook notification"}, {"type": "command", "command": "other-tool notify"}]}]},
            "permissions": {"allow": ["Bash(ls)"]}}),
    );
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let row = overrides(&result);
    assert_eq!(row["state"], "green", "{row}");
    assert_eq!(row["overrides"], json!([]), "{row}");
}

#[test]
fn a_local_settings_file_that_disables_or_replaces_bays_hooks_is_red() {
    let test_box = TestBox::new();
    let repo = installed(&test_box);
    let local = repo.join(".claude/settings.local.json");

    let text = write(&local, &json!({"disableAllHooks": true}));
    // Doctor names the file by its real path: on macOS the temporary directory is a link
    // (`/var` to `/private/var`).
    let local = local.canonicalize().unwrap();
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "HOOKS_MISSING", "{result}");
    let row = overrides(&result);
    assert_eq!(row["state"], "red", "{row}");
    let found = &row["overrides"][0];
    assert_eq!(found["file"], local.display().to_string(), "{row}");
    assert_eq!(found["disable_all_hooks"], true, "{row}");
    assert!(
        row["recovery"]
            .as_str()
            .is_some_and(|recovery| recovery.contains(&local.display().to_string())
                && recovery.contains("disableAllHooks")),
        "{row}"
    );
    // The hooks row is about the project file, which still holds bay's entries.
    assert_eq!(row_state(&result, "hooks"), "green", "{result}");
    // Doctor wrote nothing.
    assert_eq!(std::fs::read_to_string(&local).unwrap(), text);

    // Another tool's entries for one of bay's events run beside bay's (Claude Code merges the
    // hooks of every scope), so they are listed and the row stays green (R22).
    write(
        &local,
        &json!({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "other-tool check"}]}]}}),
    );
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let row = overrides(&result);
    assert_eq!(row["state"], "green", "{row}");
    assert_eq!(
        row["added"][0]["file"],
        local.display().to_string(),
        "{row}"
    );
    assert_eq!(row["added"][0]["events"], json!(["PreToolUse"]), "{row}");
    assert_ne!(result["reason"], "HOOKS_MISSING", "{result}");

    // Managed hooks only: bay's project entries do not run.
    write(&local, &json!({"allowManagedHooksOnly": true}));
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    let row = overrides(&result);
    assert_eq!(row["state"], "red", "{row}");
    assert_eq!(
        row["overrides"][0]["allow_managed_hooks_only"], true,
        "{row}"
    );
}

#[test]
fn the_user_settings_file_is_read_where_the_product_reads_it() {
    let test_box = TestBox::new();
    let repo = installed(&test_box);

    let user = test_box.home.join(".claude/settings.json");
    write(&user, &json!({"disableAllHooks": true}));
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    let row = overrides(&result);
    assert_eq!(row["state"], "red", "{row}");
    assert_eq!(
        row["overrides"][0]["file"],
        user.display().to_string(),
        "{row}"
    );
    std::fs::remove_file(&user).unwrap();

    // With CLAUDE_CONFIG_DIR set, the product reads its user settings there, not under $HOME.
    let config_dir = test_box.path().join("claude-config");
    let moved = config_dir.join("settings.json");
    write(&moved, &json!({"disableAllHooks": true}));
    write(&user, &json!({"disableAllHooks": true}));
    let (_, result) = result_json(
        test_box
            .bay(&repo)
            .env("CLAUDE_CONFIG_DIR", &config_dir)
            .arg("doctor"),
    );
    let row = overrides(&result);
    assert_eq!(row["state"], "red", "{row}");
    let files: Vec<&str> = row["overrides"]
        .as_array()
        .unwrap()
        .iter()
        .map(|found| found["file"].as_str().unwrap())
        .collect();
    assert_eq!(files, [moved.display().to_string()], "{row}");
    assert_eq!(row["overrides"][0]["disable_all_hooks"], true, "{row}");
}

fn row_state<'a>(result: &'a Value, name: &str) -> &'a Value {
    &row(result, name)["state"]
}

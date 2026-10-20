//! Doctor's `bay` row: the installed version, the newest release from the last recorded check
//! and the box's pin (`DESIGN`, the binary's lifecycle; `INTERFACES` § 8). There are no update
//! channels; a channel setting that is still set is named, never silently ignored.
//!
//! The newest release comes only from the last recorded check; doctor makes no network call
//! for it.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, result_json, row};

fn record(test_box: &TestBox, state: &serde_json::Value) {
    let dir = test_box.home.join(".local/state/bay");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("update.json"), state.to_string()).unwrap();
}

#[test]
fn the_bay_row_names_version_newest_and_pin() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");

    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let bay = row(&result, "bay");
    assert_eq!(bay["version"], bay::version::BAY, "{bay}");
    assert!(bay.get("newest").is_none(), "nothing checked yet: {bay}");
    assert_eq!(bay["pin"], serde_json::Value::Null, "{bay}");
    assert_eq!(bay["pin_source"], serde_json::Value::Null, "{bay}");
    for gone in ["channel", "channel_source", "channel_ignored"] {
        assert!(bay.get(gone).is_none(), "{gone}: {bay}");
    }
    assert_eq!(bay["state"], "green", "{bay}");

    // The pin is the box's (global Git config), never the repository's.
    test_box.commit_config(&repo, "[update]\nversion = \"0.0.1\"\n");
    record(
        &test_box,
        &serde_json::json!({"checked_at": 1, "newest": "9.1.0"}),
    );
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let bay = row(&result, "bay");
    assert_eq!(bay["pin"], serde_json::Value::Null, "{bay}");
    assert_eq!(bay["newest"], "9.1.0", "{bay}");
    assert_eq!(bay["update_available"], true, "{bay}");

    // A pinned box has no update available: only the pin moves it.
    test_box.git_ok(&repo, &["config", "--global", "bay.updateVersion", "9.0.0"]);
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let bay = row(&result, "bay");
    assert_eq!(bay["pin"], "9.0.0", "{bay}");
    assert_eq!(bay["pin_source"], "git config", "{bay}");
    assert_eq!(bay["update_available"], false, "{bay}");

    // A process's environment wins over the box's config, and the row says so.
    let (_, result) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_UPDATE_VERSION", "9.1.0")
            .arg("doctor"),
    );
    let bay = row(&result, "bay");
    assert_eq!(bay["pin"], "9.1.0", "{bay}");
    assert_eq!(bay["pin_source"], "env", "{bay}");

    // An invalid pin is named, never ignored.
    let (_, result) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_UPDATE_VERSION", "nine")
            .arg("doctor"),
    );
    let bay = row(&result, "bay");
    assert_eq!(bay["state"], "red", "{bay}");
    assert!(
        bay["why"].as_str().unwrap().contains("BAY_UPDATE_VERSION"),
        "{bay}"
    );
}

#[test]
fn a_state_file_from_the_channel_days_still_reads() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    record(
        &test_box,
        &serde_json::json!({"checked_at": 1, "channel": "stable", "newest": "9.1.0"}),
    );
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let bay = row(&result, "bay");
    assert_eq!(bay["newest"], "9.1.0", "{bay}");
    assert_eq!(bay["update_available"], true, "{bay}");
    assert!(bay.get("channel").is_none(), "{bay}");
}

#[test]
fn a_channel_setting_still_set_is_named_and_stays_green() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");

    test_box.git_ok(&repo, &["config", "--global", "bay.updateChannel", "bogus"]);
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let bay = row(&result, "bay");
    assert_eq!(bay["state"], "green", "{bay}");
    assert_eq!(bay["channel_ignored"], "bay.updateChannel", "{bay}");
    let note = bay["note"].as_str().unwrap();
    assert!(note.contains("no longer read"), "{bay}");

    test_box.git_ok(
        &repo,
        &["config", "--global", "--unset", "bay.updateChannel"],
    );
    let (_, result) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_UPDATE_CHANNEL", "stable")
            .arg("doctor"),
    );
    let bay = row(&result, "bay");
    assert_eq!(bay["state"], "green", "{bay}");
    assert_eq!(bay["channel_ignored"], "BAY_UPDATE_CHANNEL", "{bay}");
}

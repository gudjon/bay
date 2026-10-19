//! Doctor's `bay` row: the installed version, the box's channel, the newest release on that
//! channel and the box's pin (`DESIGN`, the binary's lifecycle; `INTERFACES` § 8).
//!
//! The newest release comes only from the last recorded check; doctor makes no network call
//! for it.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use support::{TestBox, result_json, row};

fn record(test_box: &TestBox, channel: &str, newest: &str) {
    let dir = test_box.home.join(".local/state/bay");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("update.json"),
        serde_json::json!({"checked_at": 1, "channel": channel, "newest": newest}).to_string(),
    )
    .unwrap();
}

#[test]
fn the_bay_row_names_version_channel_newest_and_pin() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");

    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let bay = row(&result, "bay");
    assert_eq!(bay["version"], bay::version::BAY, "{bay}");
    let default = if bay::version::BAY.contains('-') {
        "latest"
    } else {
        "stable"
    };
    assert_eq!(bay["channel"], default, "{bay}");
    assert!(bay.get("newest").is_none(), "nothing checked yet: {bay}");
    assert_eq!(bay["pin"], serde_json::Value::Null, "{bay}");
    assert_eq!(bay["pin_source"], serde_json::Value::Null, "{bay}");
    assert_eq!(bay["channel_source"], "default", "{bay}");
    assert_eq!(bay["state"], "green", "{bay}");

    // The channel and the pin are the box's (global Git config), never the repository's.
    TestBox::bay_config(
        &repo,
        "[update]\nchannel = \"latest\"\nversion = \"0.0.1\"\n",
    );
    record(&test_box, "latest", "9.1.0");
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let bay = row(&result, "bay");
    assert_eq!(
        bay["channel"], default,
        "the state file never sets the channel: {bay}"
    );
    assert_eq!(bay["pin"], serde_json::Value::Null, "{bay}");

    test_box.git_ok(
        &repo,
        &["config", "--global", "bay.updateChannel", "latest"],
    );
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let bay = row(&result, "bay");
    assert_eq!(bay["channel"], "latest", "{bay}");
    assert_eq!(bay["newest"], "9.1.0", "{bay}");
    assert_eq!(bay["update_available"], true, "{bay}");

    // A pinned box has no update available: only the pin moves it.
    test_box.git_ok(&repo, &["config", "--global", "bay.updateVersion", "9.0.0"]);
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    let bay = row(&result, "bay");
    assert_eq!(bay["pin"], "9.0.0", "{bay}");
    assert_eq!(bay["pin_source"], "git config", "{bay}");
    assert_eq!(bay["channel_source"], "git config", "{bay}");
    assert_eq!(bay["update_available"], false, "{bay}");

    // A process's environment wins over the box's config, and the row says so.
    let (_, result) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_UPDATE_VERSION", "9.1.0")
            .env("BAY_UPDATE_CHANNEL", "stable")
            .arg("doctor"),
    );
    let bay = row(&result, "bay");
    assert_eq!(bay["pin"], "9.1.0", "{bay}");
    assert_eq!(bay["pin_source"], "env", "{bay}");
    assert_eq!(bay["channel"], "stable", "{bay}");
    assert_eq!(bay["channel_source"], "env", "{bay}");

    // An invalid value is named, never ignored.
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

    // A check of another channel says nothing about this one.
    record(&test_box, "stable", "9.1.0");
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert!(row(&result, "bay").get("newest").is_none(), "{result}");
}

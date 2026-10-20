//! G17. Offline submission publishes once.
//!
//! `bay land` offline returns `queued` / `waiting_for: remote`; after reconnecting it
//! publishes exactly once, also after a crash while offline.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

fn offline(remote: &Path) -> PathBuf {
    let away = remote.with_extension("away");
    std::fs::rename(remote, &away).unwrap();
    away
}

fn online(remote: &Path, away: &Path) {
    std::fs::rename(away, remote).unwrap();
}

fn setup() -> (TestBox, PathBuf, PathBuf, String, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    (test_box, repo, remote, id, view)
}

/// After the agent reconnects (any bay command), one executor pass publishes exactly once.
fn reconnect_and_publish(test_box: &TestBox, repo: &Path, remote: &Path, id: &str) {
    test_box.bay_ok(repo, &["status", id]);
    let executor = test_box.clone_of(remote, "executor");
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "landed", "{ran}");
    let again = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(again["detail"]["ran"], serde_json::json!([]));
    assert_eq!(test_box.trunk_change_ids(remote), vec![id.to_owned()]);
}

#[test]
fn land_offline_queues_for_the_remote_then_publishes_once() {
    let (test_box, repo, remote, id, view) = setup();
    let away = offline(&remote);
    let (code, queued) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued");
    assert_eq!(queued["waiting_for"], "remote");
    assert_eq!(queued["published"], false);
    // The signed request is committed locally.
    let local = test_box.git_ok(
        &repo,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    assert!(local.contains(&format!("{id}.1")), "{local}");

    online(&remote, &away);
    reconnect_and_publish(&test_box, &repo, &remote, &id);
}

#[test]
fn land_offline_with_wait_still_queues_for_the_remote() {
    let (test_box, repo, remote, id, view) = setup();
    let away = offline(&remote);
    let (code, queued) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["waiting_for"], "remote");
    online(&remote, &away);
    reconnect_and_publish(&test_box, &repo, &remote, &id);
}

#[test]
fn a_crash_while_offline_still_publishes_once() {
    let (test_box, repo, remote, id, view) = setup();
    let away = offline(&remote);
    let killed = test_box
        .bay(&view)
        .env("BAY_TEST_KILL_AT", "after-freeze")
        .arg("land")
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), Some(137));
    online(&remote, &away);
    reconnect_and_publish(&test_box, &repo, &remote, &id);
    // The agent landing again after it all finds the publication; nothing new lands.
    let (code, again) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["decision"], "landed");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

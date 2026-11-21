//! G837 (a fleet finding, 2026-11-21: a panel that read the line from the store every 15 s found
//! its copy 106 s old, as only a status read swept the store, and kept a live read a minute to
//! keep it fresh). A store read of the line past its last sweep's window starts the sweep once it
//! has read, and leaves it running, as a store read of `bay status` does (G710); `sweep` says
//! so. Inside the window it starts none, and `sweep` is null.
//!
//! Guards to mutate: in `src/line.rs` `queue`, start no sweep (`closed` false), and the read past
//! its window says none started and the store's copy stays as old; start one whatever the window
//! (`closed` as `from_store` alone), and the read inside it starts one.

#![cfg(unix)]

use crate::support;

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use support::TestBox;

/// When the last sweep of the store at `repo` began, in Unix seconds.
fn swept_unix(repo: &Path) -> u64 {
    let text = std::fs::read_to_string(repo.join(".git/bay/swept.json")).unwrap();
    let record: Value = serde_json::from_str(&text).unwrap();
    record["unix"].as_u64().unwrap()
}

/// `bay queue --deadline 0` in `repo` with the sweep window `window_s`, once the sweep it says
/// it started has ended: the line.
fn read_line(test_box: &TestBox, repo: &Path, window_s: &str) -> Value {
    let (code, line) = support::result_json(
        test_box
            .bay(repo)
            .args(["queue", "--deadline", "0"])
            .env("BAY_TEST_SWEPT_S", window_s),
    );
    assert_eq!(code, 0, "{line}");
    if let Some(pid) = line["detail"]["sweep"]["by"]["pid"].as_u64() {
        support::process_ended(pid);
    }
    support::sweeps_ended(repo);
    line
}

/// A machine whose status swept the remote once: its store, and when that sweep began.
fn swept_store(test_box: &TestBox) -> (std::path::PathBuf, u64) {
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let remote = test_box.path().join("repo-remote.git");
    let reader = test_box.clone_of(&remote, "reader");
    let _ = test_box.bay_ok(&reader, &["status"]);
    support::sweeps_ended(&reader);
    let unix = swept_unix(&reader);
    (reader, unix)
}

#[test]
fn a_store_read_past_its_window_starts_the_sweep() {
    // GIVEN a store whose last sweep's window has closed (the window is 0 s)
    let test_box = TestBox::new();
    let (reader, before) = swept_store(&test_box);
    std::thread::sleep(Duration::from_millis(1100));

    // WHEN the line is read from the store
    let line = read_line(&test_box, &reader, "0");

    // THEN the read says it started the sweep, and the sweep ran after it
    assert_eq!(line["detail"]["from_store"], true, "{line}");
    let state = line["detail"]["sweep"]["state"].as_str();
    assert!(
        matches!(state, Some("started" | "running")),
        "the read started no sweep: {line}"
    );
    assert!(
        swept_unix(&reader) > before,
        "the store's copy is as old as before the read: {line}"
    );
}

#[test]
fn a_store_read_inside_its_window_starts_none() {
    // GIVEN a store whose last sweep's window is open (600 s)
    let test_box = TestBox::new();
    let (reader, before) = swept_store(&test_box);
    std::thread::sleep(Duration::from_millis(1100));

    // WHEN the line is read from the store
    let line = read_line(&test_box, &reader, "600");

    // THEN the read says it started none, and no sweep ran
    assert_eq!(line["detail"]["from_store"], true, "{line}");
    assert_eq!(line["detail"]["sweep"], Value::Null, "{line}");
    assert_eq!(swept_unix(&reader), before, "a sweep ran: {line}");
}

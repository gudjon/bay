//! G813 (owner ruling, 2026-11-20; a measurement: one repository's 202 session records held
//! 341 MB, 96 of them untouched for two weeks, and custody read every one in full). A start
//! removes a session record no write has touched for the retention window, with its session's
//! state folder; a record written within the window, and its folder, stay.
//!
//! Guard to mutate: in `src/start.rs` `reclaim`, drop the `prune_records` call (the old record
//! stays).

use std::time::{Duration, SystemTime};

use crate::support;

use support::{TestBox, check_files, passing_result};

fn age(path: &std::path::Path, days: u64) {
    let at = SystemTime::now() - Duration::from_secs(days * 24 * 3600);
    std::fs::File::open(path).unwrap().set_modified(at).unwrap();
}

#[test]
fn a_start_removes_the_session_records_the_window_left_behind() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let bay = repo.join(".git/bay");
    let records = bay.join("sessions");
    let states = bay.join("session-state");
    std::fs::create_dir_all(&records).unwrap();
    // An old session (8 days, past the default 7-day window) and a fresh one.
    let line = "{\"schema\":\"bay.session/1\",\"actor\":{\"name\":\"agent\"}}\n";
    let old = records.join("s-old.jsonl");
    let fresh = records.join("s-fresh.jsonl");
    std::fs::write(&old, line).unwrap();
    std::fs::write(&fresh, line).unwrap();
    let old_state = states.join("s-old");
    let fresh_state = states.join("s-fresh");
    std::fs::create_dir_all(&old_state).unwrap();
    std::fs::create_dir_all(&fresh_state).unwrap();
    age(&old, 8);
    age(&old_state, 8);

    // WHEN a change starts
    let started = test_box.bay_ok(&repo, &["start", "another", "--paths", "src", "--new"]);

    // THEN the old record and its state folder are gone, and the fresh ones stay
    assert!(!old.exists(), "a record past the window stayed: {started}");
    assert!(!old_state.exists(), "its state folder stayed: {started}");
    assert!(fresh.exists(), "a record within the window went: {started}");
    assert!(
        fresh_state.exists(),
        "a live session's state folder went: {started}"
    );
}

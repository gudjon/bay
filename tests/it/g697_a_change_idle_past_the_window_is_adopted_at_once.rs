//! G697 (a fleet finding, 2026-11-11: taking over 38 changes whose actors were gone for days
//! took about four minutes each). An adoption watches the change for the takeover window, so
//! that a working actor keeps its change. A change whose chain was last written longer than the
//! window ago, with no heartbeat of its actor naming it within the window, already shows its
//! actor idle that long: `bay start <change> --adopt` takes it at once. A change written within
//! the window is still watched for the full window (G36).
//!
//! Guard to mutate: in `src/start.rs` `holder_works`, the return on a chain idle past the window
//! (the adoption waits out the whole window).

#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

/// The takeover window: long enough that a watch of it fails the test's bound.
const TAKEOVER_S: u64 = 60;

#[test]
fn a_change_idle_past_the_window_is_adopted_at_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        format!("[runner]\ntakeover_s = {TAKEOVER_S}\n"),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);

    // GIVEN a change its actor opened a day ago and left
    let day_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 86_400;
    let date = format!("@{day_ago} +0000");
    let (code, started) = result_json(
        test_box
            .bay(&repo)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_AUTHOR_DATE", &date)
            .args(["start", "CARD-GONE", "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{started}");
    let id = started["change"].as_str().unwrap().to_owned();

    // WHEN another actor takes it over
    let other = test_box.clone_of(&remote, "other");
    let began = Instant::now();
    let (code, adopted) = result_json(
        test_box
            .bay(&other)
            .env("BAY_ACTOR", "agent-b")
            .args(["start", &id, "--adopt"]),
    );

    // THEN it is theirs at once, well inside the window
    assert_eq!(code, 0, "{adopted}");
    assert_eq!(adopted["detail"]["adopted"], true, "{adopted}");
    assert!(
        began.elapsed() < Duration::from_secs(TAKEOVER_S / 2),
        "the adoption took {:?}, watching the window of a change idle for a day",
        began.elapsed()
    );
}

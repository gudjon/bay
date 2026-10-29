//! G561 (BACKLOG #129; a fleet finding, 2026-10-29: a runner wrote nothing for the 27
//! minutes one post check ran, so its silence read the same as a stall). A check that runs
//! longer than the still interval says so in the runner's journal, once each interval, naming
//! the check and the commit; a check that ends sooner writes nothing.
//!
//! Guard to mutate, in `src/check.rs`: `run_once` calls `within_budget` without `saying_still`
//! (the long check writes no still line).

// The check is a shell script, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

/// The runner's journal for one `--once` pass over one change whose check runs `before` first.
fn journal(before: &str) -> String {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files(before, &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nheartbeat_s = 1\nlease_s = 30\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let runner = test_box.clone_of(&remote, "runner");
    let output = test_box
        .bay_process(&runner)
        .args(["runner", "run", "--once"])
        .env("BAY_TEST_STILL_EVERY_S", "1")
        .output()
        .unwrap();
    let journal = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{journal}");
    journal
}

#[test]
fn a_check_longer_than_the_interval_names_itself_each_interval() {
    let journal = journal("sleep 3");
    let still: Vec<&str> = journal
        .lines()
        .filter(|line| {
            line.starts_with("bay: check tests on ") && line.contains(" still running (")
        })
        .collect();
    assert!(
        still.len() >= 2,
        "a check of 3 s under a 1 s interval said it still ran {} times: {journal}",
        still.len()
    );
}

#[test]
fn a_check_shorter_than_the_interval_writes_no_still_line() {
    let journal = journal("true");
    assert!(
        !journal.contains("still running"),
        "a check that ended within the interval said it still ran: {journal}"
    );
}

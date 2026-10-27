//! G414 (a fleet finding, 2026-10-27: a runner's pass made the retention cut of each closed chain
//! that was due inside its loop over changes, a fetch of each chain first, and a cut that did not
//! happen left the chain due on every later pass; with hundreds of closed chains the runner was
//! silent for 8 to 24 minutes ahead of every change sorted after them, and nothing was admitted).
//! Retention cuts come after the queue's work, and only while the pass is within its bound.
//!
//! Guard to mutate: `src/executor.rs` `pass`: the retention cut back inside the loop over changes.

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_pass_past_its_bound_runs_the_queue_and_cuts_nothing() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let runner = test_box.clone_of(&remote, "runner");
    // A lands, so its chain is due for its cut at the next pass.
    let (first, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    test_box.bay_ok(&runner, &["runner", "run", "--once"]);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![first]);
    // B waits in line.
    let (second, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");

    let (code, pass) = result_json(
        test_box
            .bay(&runner)
            .env("BAY_TEST_PASS_BOUND_S", "0")
            .args(["runner", "run", "--once"]),
    );
    assert_eq!(code, 0, "{pass}");
    let ran = pass["detail"]["ran"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        ran.iter()
            .any(|row| row["change"] == second.as_str() && row.get("op_id").is_some()),
        "the waiting change ran: {pass}"
    );
    assert!(
        ran.iter().all(|row| row.get("retention_cut").is_none()),
        "a pass past its bound made a cut: {pass}"
    );
}

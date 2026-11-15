//! G741 (a fleet finding, 2026-11-15: a remote held 1,127 queue places and 1,126 releases with one
//! place live, kept for the whole retention window, and listing them took 43 s). A runner's queue
//! retention removes a finished operation's (reservation, release) pair once its release is an
//! hour old, whatever the retention window, below the head and never the highest pair (§ 10a).
//!
//! Guard to mutate: in `src/executor.rs` `queue_retention`, keep the whole retention window, and
//! no pair two hours old is removed under the default window of seven days.

use crate::g200b_executor_cost_stays_flat_with_history::{history, pass, queue_refs};

use serde_json::Value;

#[test]
fn a_finished_pair_released_two_hours_ago_is_removed_under_the_default_window() {
    let (test_box, remote, executor) = history(5);
    // GIVEN releases made two hours ago: each release commit again, dated then
    let two_hours_ago = format!(
        "{} +0000",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 7200
    );
    for seq in 1..=5 {
        let name = format!("refs/bay/queue-done/{seq}");
        let old = test_box.git_bare(&remote, &["rev-parse", &name]);
        let tree = test_box.git_bare(&remote, &["rev-parse", &format!("{old}^{{tree}}")]);
        let message = test_box.git_bare(&remote, &["log", "-1", "--format=%B", &old]);
        let mut args = vec!["commit-tree".to_owned(), tree, "-m".to_owned(), message];
        for parent in test_box
            .git_bare(&remote, &["rev-parse", &format!("{old}^@")])
            .lines()
        {
            args.push("-p".to_owned());
            args.push(parent.to_owned());
        }
        let out = test_box
            .git(&remote)
            .args(&args)
            .env("GIT_COMMITTER_DATE", &two_hours_ago)
            .env("GIT_AUTHOR_DATE", &two_hours_ago)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let new = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        test_box.git_bare(&remote, &["update-ref", &name, &new, &old]);
    }

    // WHEN a runner passes under the default retention window
    let (_, ran) = pass(&test_box, &executor, "retention");

    // THEN the finished pairs below the highest are removed
    let pruned: Vec<u64> = ran["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|step| step["queue_pruned"].as_array().cloned())
        .unwrap_or_else(|| panic!("no queue retention: {ran}"))
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    assert_eq!(pruned, vec![1, 2, 3, 4], "{ran}");
    assert_eq!(queue_refs(&test_box, &remote).len(), 3);
}

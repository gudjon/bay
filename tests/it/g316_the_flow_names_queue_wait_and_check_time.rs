//! G316. The flow names queue wait and check time.
//!
//! A team measures itself by how long a change waits for its turn and how long its checks run.
//! `bay status` names both under `detail.flow`, beside the cycle time, over the changes landed
//! in the last 30 days: `queue_wait_s` `{p50, p90, landed}` and `check_time_s`
//! `{p50, p90, runs}`, in seconds, null when nothing landed. They are derived on read from the
//! one home of an operation's timing, its published terminal record (G253): the queue wait is
//! that record's `queue_wait_s`, and each check run is one of its attempt rows, so the flow and
//! the operation row never disagree.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/flow.rs` `flow`: `"queue_wait_s": queue_wait(&published),` (drop it: there is no
//!   `queue_wait_s`);
//! - `src/flow.rs` `flow`: `"check_time_s": check_time(&published),` (drop it: there is no
//!   `check_time_s`);
//! - `src/flow.rs` `queue_wait`: `.filter_map(|record| record["queue_wait_s"].as_u64())` (make
//!   it `record["head_at"]`: the percentiles are Unix times, not the records' queue waits);
//! - `src/flow.rs` `check_time`: `.filter_map(|row| row["seconds"].as_u64())` (make it
//!   `row["started_at"]`: the percentiles are Unix times, not the attempt rows' seconds);
//! - `src/log.rs` `published_of`: `.is_some_and(|(change, _)| changes.contains(change))` (make
//!   it `!changes.contains(change)`: no record is read, and both are null);
//! - `src/log.rs` `published_of`: `record["outcome"] == "published"` (make it `!=`: both are
//!   null).

// The check is a shell script, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// The published terminal record of change `id`, as `bay status <id>` shows it.
fn terminal(test_box: &TestBox, repo: &std::path::Path, id: &str) -> Value {
    let status = test_box.bay_ok(repo, &["status", id]);
    status["detail"]["changes"][0]["terminal"]
        .as_array()
        .unwrap_or_else(|| panic!("no terminal records: {status}"))
        .iter()
        .find(|record| record["outcome"] == "published")
        .unwrap_or_else(|| panic!("no published terminal record: {status}"))
        .clone()
}

/// The `p50` and `p90` of `spread`, each a number of seconds.
fn p50_p90(spread: &Value) -> (u64, u64) {
    let p50 = spread["p50"]
        .as_u64()
        .unwrap_or_else(|| panic!("no p50: {spread}"));
    let p90 = spread["p90"]
        .as_u64()
        .unwrap_or_else(|| panic!("no p90: {spread}"));
    assert!(p50 <= p90, "{spread}");
    (p50, p90)
}

#[test]
fn status_names_the_queue_wait_and_check_time_of_landed_changes() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let executor = test_box.clone_of(&remote, "executor");

    // Nothing landed yet: neither is named with a number.
    let before = test_box.bay_ok(&repo, &["status"]);
    let flow = &before["detail"]["flow"];
    assert!(flow["queue_wait_s"].is_null(), "{before}");
    assert!(flow["check_time_s"].is_null(), "{before}");

    let mut ids = Vec::new();
    for name in ["one", "two"] {
        let (id, view) = test_box.start_and_edit(&repo);
        // Each change writes bytes of its own, so the second is not `no_effect` (G213).
        std::fs::write(
            view.join(format!("src/{name}.rs")),
            format!("fn {name}() {{}}\n"),
        )
        .unwrap();
        let (code, submitted) = result_json(test_box.bay(&view).arg("merge"));
        assert_eq!(code, 0, "{submitted}");
        let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
        assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
        ids.push(id);
    }

    let status = test_box.bay_ok(&repo, &["status"]);
    let flow = &status["detail"]["flow"];
    let queue_wait = &flow["queue_wait_s"];
    assert_eq!(queue_wait["landed"], 2, "{status}");
    let (wait_p50, wait_p90) = p50_p90(queue_wait);
    let check_time = &flow["check_time_s"];
    let (check_p50, check_p90) = p50_p90(check_time);
    let runs = check_time["runs"]
        .as_u64()
        .unwrap_or_else(|| panic!("no runs: {status}"));
    assert!(runs >= 2, "one run per landing at least: {status}");

    // The same facts as each operation's terminal record holds: by nearest rank, of two queue
    // waits the p50 is the smaller and the p90 the larger.
    let records: Vec<Value> = ids
        .iter()
        .map(|id| terminal(&test_box, &repo, id))
        .collect();
    let mut waits: Vec<u64> = records
        .iter()
        .map(|record| {
            record["queue_wait_s"]
                .as_u64()
                .unwrap_or_else(|| panic!("no queue_wait_s: {record}"))
        })
        .collect();
    waits.sort_unstable();
    assert_eq!((wait_p50, wait_p90), (waits[0], waits[1]), "{status}");
    let mut seconds: Vec<u64> = records
        .iter()
        .flat_map(|record| record["checks"].as_array().cloned().unwrap_or_default())
        .filter_map(|row| row["seconds"].as_u64())
        .collect();
    seconds.sort_unstable();
    assert_eq!(seconds.len(), usize::try_from(runs).unwrap(), "{status}");
    assert_eq!(
        (check_p50, check_p90),
        (nearest(&seconds, 50), nearest(&seconds, 90)),
        "{status}"
    );
}

/// The `percent`th percentile of `sorted` (ascending, not empty), by nearest rank.
fn nearest(sorted: &[u64], percent: usize) -> u64 {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

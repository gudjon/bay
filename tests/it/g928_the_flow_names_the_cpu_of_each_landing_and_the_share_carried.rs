//! G928 (plan row S5, "CPU per landing" and a hit-rate counter per tier): each check record
//! names the CPU seconds of its adapter's processes (`cpu_s`), measured by bay around the run,
//! and whether it was carried from another candidate rather than run (`carried`). The terminal
//! record's runs carry both, and `bay status` names under `detail.flow` the CPU of each landing's
//! checks (`cpu_s` `{p50, p90, merged, sum}`) and how many check records were carried
//! (`carried` `{runs, carried}`).
//!
//! Guards to mutate (each makes the case fail):
//! - `src/run.rs` `bounded_until`: `cpu = match (tree_cpu(group), cpu) {` (read the leader alone,
//!   `stat.map(|(_, used)| used)`: under `bwrap --unshare-pid` the check's CPU is missed);
//! - `src/flow.rs` `flow`: `"cpu_s": cpu_per_landing(&published),` (drop it);
//! - `src/flow.rs` `carried_runs`: `.filter(|run| run["carried"].is_boolean())` (make it
//!   `is_null()`: no run counts, and `carried` is null).

// CPU times are read from Linux's /proc, so this runs on Linux only.
#![cfg(target_os = "linux")]

use crate::support;

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// A check that spins for a while before it answers.
const SPIN: &str = "i=0; while [ $i -lt 300000 ]; do i=$((i+1)); done";

/// The published terminal record of change `id`, as `bay status <id>` shows it.
fn terminal(test_box: &TestBox, repo: &std::path::Path, id: &str) -> Value {
    let status = test_box.bay_ok(repo, &["status", id]);
    status["detail"]["changes"][0]["terminal"]
        .as_array()
        .unwrap_or_else(|| panic!("no terminal records: {status}"))
        .iter()
        .find(|record| record["outcome"] == "merged")
        .unwrap_or_else(|| panic!("no published terminal record: {status}"))
        .clone()
}

#[test]
fn the_flow_names_the_cpu_of_each_landing_and_the_share_of_check_records_carried() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(SPIN, &passing_result(1), 1, 60),
    );
    let executor = test_box.clone_of(&remote, "executor");

    // GIVEN nothing landed: neither is named
    let before = test_box.bay_ok(&repo, &["status"]);
    assert!(before["detail"]["flow"]["cpu_s"].is_null(), "{before}");
    assert!(before["detail"]["flow"]["carried"].is_null(), "{before}");

    // WHEN a change lands, its check spinning on the runner
    let (id, view) = test_box.start_and_edit(&repo);
    std::fs::write(view.join("src/one.rs"), "fn one() {}\n").unwrap();
    let (code, submitted) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{submitted}");
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");

    // THEN its terminal record's run names the CPU the spin used, run here, not carried
    let record = terminal(&test_box, &repo, &id);
    let runs = record["checks"]
        .as_array()
        .and_then(|rows| rows.last())
        .and_then(|row| row["runs"].as_array())
        .cloned()
        .unwrap_or_else(|| panic!("no runs: {record}"));
    assert!(!runs.is_empty(), "{record}");
    let cpu: f64 = runs
        .iter()
        .map(|run| {
            run["cpu_s"]
                .as_f64()
                .unwrap_or_else(|| panic!("no cpu_s: {record}"))
        })
        .sum();
    assert!(cpu >= 0.1, "the spin used no CPU: {record}");
    assert!(runs.iter().all(|run| run["carried"] == false), "{record}");

    // AND the flow names that landing's CPU, and no record carried
    let status = test_box.bay_ok(&repo, &["status"]);
    let flow = &status["detail"]["flow"];
    let cpu_s = &flow["cpu_s"];
    assert_eq!(cpu_s["merged"], 1, "{status}");
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let whole = cpu.round() as u64;
    assert_eq!(cpu_s["p50"], whole, "{status}");
    assert_eq!(cpu_s["sum"], whole, "{status}");
    assert_eq!(flow["carried"]["runs"], runs.len(), "{status}");
    assert_eq!(flow["carried"]["carried"], 0, "{status}");
}

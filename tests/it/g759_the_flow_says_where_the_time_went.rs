//! G759 (owner, 2026-11-16: "make sure we are doing all the right measurements to measure the
//! velocity, throughput, each wait time and other so we can learn where to best improve"; by
//! hand, 82% of six hours' cycle time on one fleet was the wait before a change's work counted,
//! and a week's percentiles hid it). Each landing's timing record says how many claims it took,
//! the time between its first and last claim (work done again), the time from its last claim to
//! main, and why its work was given back, by cause; `bay status` names the last six hours of
//! landings under `detail.flow.recent`: how many, how many an hour, the spread of each part and
//! each part's share of the time.
//!
//! Guards to mutate: in `src/flow.rs` `flow`, drop `"recent"`; in `src/timing.rs` `of`, drop the
//! `final_s` field (the shares no longer sum to their total).

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn the_flow_says_where_the_time_of_recent_landings_went() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let executor = test_box.clone_of(&remote, "executor");
    let mut ids = Vec::new();
    for name in ["one", "two"] {
        let (id, view) = test_box.start_and_edit(&repo);
        std::fs::write(
            view.join(format!("src/{name}.rs")),
            format!("fn {name}() {{}}\n"),
        )
        .unwrap();
        let (code, submitted) = result_json(
            test_box
                .bay(&view)
                .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
                .arg("merge"),
        );
        assert_eq!(code, 0, "{submitted}");
        let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
        assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
        ids.push(id);
    }

    // WHEN the flow is read
    let status = test_box.bay_ok(&repo, &["status"]);
    let recent = &status["detail"]["flow"]["recent"];

    // THEN it names the recent landings, their pace, and where their time went
    assert_eq!(recent["window_s"], 21600, "{status}");
    let landed = recent
        .get("landed")
        .or_else(|| recent.get("merged"))
        .and_then(serde_json::Value::as_u64);
    assert_eq!(landed, Some(2), "{status}");
    for part in ["total_s", "queue_wait_s", "rework_s", "final_s"] {
        assert!(recent[part]["p50"].as_u64().is_some(), "{part}: {status}");
    }
    let share = &recent["share_pct"];
    let summed: u64 = ["queue_wait", "rework", "final"]
        .iter()
        .map(|part| share[*part].as_u64().unwrap_or(0))
        .sum();
    assert!((99..=101).contains(&summed) || summed == 0, "{status}");
    for cause in ["main_moved", "member_left", "other", "yields"] {
        assert_eq!(
            recent["rework"][cause], 0,
            "nothing was given back: {status}"
        );
    }

    // AND each landing's own record carries its claims and the time from its last claim to main
    let one = test_box.bay_ok(&repo, &["status", &ids[0]]);
    let record = one["detail"]["changes"][0]["terminal"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["outcome"] == "merged")
        .unwrap()
        .clone();
    assert_eq!(record["claims"], 1, "{record}");
    assert!(record["final_s"].as_u64().is_some(), "{record}");
    assert_eq!(record["rework_s"], 0, "{record}");
}

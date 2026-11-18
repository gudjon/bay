//! G786c (owner, 2026-11-18: the estimate of when a change is on main must be visible as right
//! or wrong, so it improves). `detail.flow.recent.estimates` replays, for each recent landing,
//! the estimate it would have been given when it was sent and when it joined the line, from
//! only the landings before that moment, and says how many it could judge, the share whose range
//! held when it reached main, the shares that came earlier and later, and the median error.
//!
//! Guard to mutate: in `src/flow.rs` `lately`, drop `"estimates"`.

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn the_flow_says_how_well_its_estimates_held() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let executor = test_box.clone_of(&remote, "executor");
    // GIVEN four landings, one after another: the last has three before it to be judged by
    for name in ["one", "two", "three", "four"] {
        let (_, view) = test_box.start_and_edit(&repo);
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
        // Each is sent after the one before it reached main.
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    // WHEN the flow is read
    let status = test_box.bay_ok(&repo, &["status"]);
    let held = &status["detail"]["flow"]["recent"]["estimates"];

    // THEN it says how the estimate given at sending held for the landings it could judge
    let sent = &held["sent"];
    assert!(
        sent["cases"].as_u64().is_some_and(|cases| cases >= 1),
        "{status}"
    );
    let shares: u64 = ["inside_pct", "early_pct", "late_pct"]
        .iter()
        .map(|share| sent[*share].as_u64().unwrap())
        .sum();
    assert!((99..=101).contains(&shares), "{status}");
    assert!(sent["median_error_s"].as_u64().is_some(), "{status}");
    assert!(held.get("in_line").is_some(), "{status}");
}

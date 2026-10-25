//! G362 (owner ruling 2026-10-25, "yes, fix these": cheap checks first). A failed check ends the
//! run.
//!
//! A check whose last run is a `product_failure` ends the candidate's run: no check after it in
//! the policy's order starts, so a seconds-long lint that fails never pays for the slow tests
//! behind it. The merge is refused `CHECK_FAILED`, naming the check that failed, and each check
//! that never started as `not_run`. Checks that
//! pass let every later check run, as before. With `[runner] parallel_checks`, a check already
//! running finishes, and none starts after the failure.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/check.rs` `run_all`: drop the `ended_by_failure` break (the serial run starts the
//!   tests after the failed lint).
//! - `src/check.rs` `run_parallel`: drop the `ended_by_failure` term of `stop` (the parallel
//!   run starts the third check after the lint failed).

// Each case runs a check adapter (a shell script), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use support::{TestBox, passing_result, result_json};

/// One adapter for every check, its mode the first argument: `lint` fails when the candidate
/// holds `src/lint.bad`, `slow` passes after two seconds, any other mode passes. Each run logs
/// its mode to the returned file, through the relay.
fn trunk(test_box: &TestBox, checks: &[&str], runner: &str) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let log = test_box.path().join("runs.log");
    let logged = |mode: &str| test_box.relay(&format!("echo {mode} >> '{}'", log.display()));
    let fail = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let pass = passing_result(1);
    let script = format!(
        "#!/bin/sh\nreq=$(cat)\ncommit=$(printf '%s' \"$req\" | sed -n 's/.*\"candidate\":{{\"commit\":\"\\([0-9a-f]*\\)\",\"tree\":\"\\([0-9a-f]*\\)\"}}.*/\\1/p')\ntree=$(printf '%s' \"$req\" | sed -n 's/.*\"candidate\":{{\"commit\":\"\\([0-9a-f]*\\)\",\"tree\":\"\\([0-9a-f]*\\)\"}}.*/\\2/p')\ncase \"$1\" in\n  lint) {lint_log}\n    if [ -f src/lint.bad ]; then {fail}; else {pass}; fi ;;\n  slow) {slow_log}\n    sleep 2; {pass} ;;\n  *) {other_log}\n    {pass} ;;\nesac\nexit 0\n",
        lint_log = logged("lint"),
        slow_log = logged("slow"),
        other_log = logged("$1"),
    );
    let mut policy = String::from("revision = 1\n");
    for mode in checks {
        let _ = write!(
            policy,
            "\n[[check]]\nid = \"{mode}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check {mode}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n"
        );
    }
    let mut files = vec![
        ("scripts/bay-check".to_owned(), script),
        (".bay/policy.toml".to_owned(), policy),
    ];
    if !runner.is_empty() {
        files.push((
            ".bay/config.toml".to_owned(),
            format!("[runner]\n{runner}\n"),
        ));
    }
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (repo, log)
}

/// The modes the adapter ran, in order.
fn ran(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_failed_lint_ends_the_run_before_the_tests_start() {
    let test_box = TestBox::new();
    let (repo, log) = trunk(&test_box, &["lint", "tests"], "");
    let (_, view) = test_box.start_and_edit(&repo);
    std::fs::write(view.join("src/lint.bad"), "x\n").unwrap();
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    assert!(
        refused.to_string().contains("lint"),
        "the failed check is not named: {refused}"
    );
    assert_eq!(ran(&log), ["lint"], "a check ran after the failed lint");
    // The check the failure kept from starting is named, never passed.
    let rows = refused["detail"]["checks"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        rows.iter()
            .any(|row| row["check"] == "tests" && row["verdict"] == "not_run"),
        "tests is not named not_run: {refused}"
    );
}

#[test]
fn checks_that_pass_let_every_later_check_run() {
    let test_box = TestBox::new();
    let (repo, log) = trunk(&test_box, &["lint", "tests"], "");
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    assert_eq!(ran(&log), ["lint", "tests"]);
}

#[test]
fn in_a_parallel_run_no_check_starts_after_a_failure() {
    let test_box = TestBox::new();
    // Two at once: lint fails at once while slow runs; the third must never start.
    let (repo, log) = trunk(&test_box, &["lint", "slow", "tests"], "parallel_checks = 2");
    let (_, view) = test_box.start_and_edit(&repo);
    std::fs::write(view.join("src/lint.bad"), "x\n").unwrap();
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    let mut modes = ran(&log);
    modes.sort();
    assert_eq!(
        modes,
        ["lint", "slow"],
        "a check started after the failed lint"
    );
}

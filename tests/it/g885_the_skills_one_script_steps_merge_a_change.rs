//! G885 (a measurement, 2026-11-24: a change merged on one fleet took a median of 24 tool batches,
//! 13 in sessions that ran most commands from scripts): the skill's "One script per change" names
//! the steps one script runs, and those steps, run as written, merge a change.
//!
//! Guard to mutate: `skills/bay/SKILL.md`, step 3 back to `bay --json merge` with no change: the
//! steps no longer name the commands this test runs.

// The check runs a shell script, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::{TestBox, check_files, passing_result, result_json};

const SKILL: &str = include_str!("../../skills/bay/SKILL.md");

#[test]
fn the_skills_one_script_steps_merge_a_change() {
    // GIVEN the skill's steps for one script
    let steps = SKILL
        .split("## One script per change")
        .nth(1)
        .expect("the skill has the section")
        .split("\n## ")
        .next()
        .unwrap();
    for named in [
        "`bay --json start <intent> --paths <paths>`",
        "`change` and `detail.view`",
        "`git -C <view> add -- <paths>`",
        "`git -C <view> commit -q -m <message>`",
        "`bay --json merge <change>`",
        "`bay --json merge <change> --wait=3600` as a background task",
        "`next_step`",
    ] {
        assert!(
            steps.contains(named),
            "the steps do not name {named}: {steps}"
        );
    }
    // AND a repository with one check, and a runner's clone
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let runner = test_box.clone_of(&remote, "runner");

    // WHEN a script runs the steps as written, from the repository
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let change = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/b.rs"), "fn b() {}\n").unwrap();
    test_box.git_ok(&view, &["add", "--", "src/"]);
    test_box.git_ok(&view, &["commit", "-q", "-m", "add b"]);
    let (code, submitted) = result_json(test_box.bay(&repo).args(["merge", &change]));
    assert_eq!(code, 0, "{submitted}");
    assert_eq!(submitted["decision"], "queued", "{submitted}");
    let ran = test_box.bay_ok(&runner, &["runner", "run", "--once"]);
    let (code, waited) = result_json(test_box.bay(&repo).args(["merge", &change, "--wait=60"]));

    // THEN the wait answers merged, and main holds the file
    assert_eq!(code, 0, "{waited} | {ran}");
    assert_eq!(waited["decision"], "merged", "{waited} | {ran}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![change]);
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/b.rs"]),
        "fn b() {}"
    );
}

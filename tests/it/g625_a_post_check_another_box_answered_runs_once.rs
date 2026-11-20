//! G625 (E87, a fleet finding, 2026-11-01: two runner boxes served one environment, and each ran
//! every post check on each tip: 62 runs on 46 tips in three hours, every verdict the same, 36 of
//! them after the other box had recorded its answer). A box plans a post check on main's tip only
//! when no other producer's record answers it there, and asks about what changed since any
//! producer last answered it.
//!
//! Guards to mutate: `elsewhere.contains(&id)` in `bisect::tips` (`src/bisect.rs`); drop it and
//! the second box runs the check the first one answered. (The check has no `when_paths`, so the
//! empty range since that answer still wakes it: only this guard stops the run.) The `elsewhere`
//! chain in `bisect::last_runs`; drop it and the second box's run on the next tip asks about every
//! landing since the span's start, not since the first box's answer.
//!
//! The first box owns `suite` by G822's rule, so the second box runs it on the next tip only once
//! the first box reads gone: that pass reads the clock an hour on.

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// Lands a change that writes `path` in `dir`.
fn land(test_box: &TestBox, repo: &Path, dir: &str, path: &str) {
    let started = test_box.bay_ok(repo, &["start", path, "--paths", dir]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
}

/// One runner pass with the reader's clock `skew_s` ahead: the post check it ran, or `""`.
fn ran_post(test_box: &TestBox, repo: &Path, skew_s: u64) -> String {
    let (code, ran) = result_json(
        test_box
            .bay(repo)
            .args(["runner", "run", "--once"])
            .env("BAY_TEST_PRESENCE_CLOCK_SKEW_S", skew_s.to_string()),
    );
    assert_eq!(code, 0, "{ran}");
    ran["detail"]["ran"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|row| row.get("post"))
        .and_then(|post| post[0]["check"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[test]
fn a_post_check_another_box_answered_on_the_tip_runs_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    // Each run of the post check `suite` logs the paths its request names as changed.
    let log = test_box.relay(&format!(
        "changed=$(printf '%s' \"$req\" | sed -n 's/.*\"changed_paths\":\\[\\([^]]*\\)\\].*/\\1/p')\necho \"suite $changed\" >> '{}'",
        runs.display()
    ));
    let mut files = check_files(
        &format!("[ \"$1\" = suite ] && {log}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str("\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n");
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\nbisect_post = false\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    // A second runner box on the same remote, which serves the same environment.
    let other = test_box.clone_of(&remote, "other");
    test_box.git_ok(&other, &["config", "bay.runnerName", "other"]);
    test_box.git_ok(&other, &["config", "bay.producerName", "other-checks"]);

    land(&test_box, &repo, "src", "src/s0.rs");
    assert_eq!(ran_post(&test_box, &repo, 0), "suite");
    assert_eq!(
        ran_post(&test_box, &other, 0),
        "",
        "the other box ran a check the first one had answered on this tip"
    );
    land(&test_box, &repo, "src", "src/s1.rs");
    assert_eq!(ran_post(&test_box, &other, 3600), "suite");
    assert_eq!(
        ran_post(&test_box, &repo, 0),
        "",
        "the first box ran a check the other one had answered on this tip"
    );

    let text = std::fs::read_to_string(&runs).unwrap_or_default();
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("suite"))
        .collect();
    assert_eq!(lines.len(), 2, "two tips, one run each: {text}");
    // The other box's run asks about what changed since the first box answered, not since the
    // span's start.
    assert!(
        lines[1].contains("src/s1.rs") && !lines[1].contains("src/s0.rs"),
        "the other box asked about landings the first one had answered: {text}"
    );
}

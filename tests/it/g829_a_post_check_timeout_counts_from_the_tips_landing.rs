//! G829 (a fleet finding, 2026-11-20: a check last answered 14 min before a new tip landed ran on
//! the tip on both runners of its environment, 4 min after it landed, under a 15 min timeout).
//! G822 lets a runner that does not own a post check run it once the check's timeout has passed
//! without an answer, and counted that time from the check's newest answer alone. It counts from
//! the later of that answer and the landing of main's tip.
//!
//! Guard to mutate: the tip's landing time in the timeout test of `bisect::left_to`
//! (`src/bisect.rs`); count from the newest answer alone and the runner that does not own the
//! check runs it beside its live owner on a tip that landed after the check last answered.

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use support::{EXECUTOR, TestBox, check_files, passing_result, result_json};

use crate::g822_a_post_check_runs_on_one_live_runner::{ran_post, weight};

/// The check's timeout, and how far ahead of the clock the second tip's landing is dated: as far
/// as the timeout, and well inside the minute a runner's presence reads live (three of its 10 s
/// cadences and half a minute). The second landing and the next pass have the whole timeout to
/// finish in, on a loaded box too (a 10 s one ran out in a full suite, 2026-11-22).
const TIMEOUT_S: u64 = 25;
const AHEAD_S: u64 = 25;

/// Two runner clones of one remote that serve one environment, and the post check `suite` with
/// a timeout of [`TIMEOUT_S`]. Returns the clone that owns `suite`, the clone that does not,
/// and the file each run of `suite` writes a line to.
fn two_runners(test_box: &TestBox) -> (PathBuf, PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    let log = test_box.relay(&format!("echo suite >> '{}'", runs.display()));
    let mut files = check_files(
        &format!("[ \"$1\" = suite ] && {log}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            let _ = write!(
                text,
                "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = {TIMEOUT_S}\n"
            );
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\nbisect_post = false\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let other = test_box.clone_of(&remote, "other");
    test_box.git_ok(&other, &["config", "bay.runnerName", "other"]);
    test_box.git_ok(&other, &["config", "bay.producerName", "other-checks"]);
    if weight(EXECUTOR) > weight("other") {
        (repo, other, runs)
    } else {
        (other, repo, runs)
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Lands a change that writes `path` in `dir`, its commits dated `at` (Unix seconds).
fn land_at(test_box: &TestBox, repo: &Path, dir: &str, path: &str, at: u64) {
    let date = format!("@{at} +0000");
    let started = test_box.bay_ok(repo, &["start", path, "--paths", dir]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    let (code, landed) = result_json(
        test_box
            .bay(&view)
            .env("GIT_COMMITTER_DATE", &date)
            .env("GIT_AUTHOR_DATE", &date)
            .args(["merge", "--wait"]),
    );
    assert_eq!(code, 0, "{landed}");
}

fn runs_of(runs: &Path) -> usize {
    std::fs::read_to_string(runs)
        .unwrap_or_default()
        .lines()
        .count()
}

#[test]
fn a_post_check_answered_before_its_timeout_ago_waits_on_its_owner_for_a_tip_that_just_landed() {
    let test_box = TestBox::new();
    let (owner, other, runs) = two_runners(&test_box);

    // GIVEN a post check its owner answered on main's tip
    land_at(&test_box, &owner, "src", "src/s0.rs", unix_now());
    assert_eq!(ran_post(&test_box, &owner, 0), "suite");

    // AND a new tip that lands later than the check's timeout after that answer
    let landed = unix_now() + AHEAD_S;
    land_at(&test_box, &owner, "src", "src/s1.rs", landed);
    let tip = test_box.git_ok(&owner, &["log", "-1", "--format=%ct", "origin/main"]);
    assert_eq!(
        tip.trim(),
        landed.to_string(),
        "the tip is dated as the test means"
    );

    // WHEN the runner that does not own the check passes just after the tip landed
    // THEN it leaves the check to its live owner
    assert_eq!(
        ran_post(&test_box, &other, AHEAD_S),
        "",
        "a runner ran a post check its live owner had not yet had its timeout to answer on the tip"
    );
    assert_eq!(ran_post(&test_box, &owner, 0), "suite");
    assert_eq!(runs_of(&runs), 2, "two tips, one run each");
}

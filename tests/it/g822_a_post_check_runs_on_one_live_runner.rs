//! G822 (a fleet finding, 2026-11-19: three runners served one environment, and each ran every
//! post check on each tip it found unanswered; 23.0 of 49.1 post runner-hours in a day were second
//! and third runs of one check on one tip, started before the first run had answered). Of the live
//! runners that serve a post check's environment (and that the check's producers name, where it
//! names any), one owns the check: the one with the highest weight from the check, its environment
//! and the runner's name. Another runner runs the check only when no producer answered it in main's
//! newest landings, when its newest answer is as old as its timeout, or when the owner's presence
//! reads stale.
//!
//! Guards to mutate: the `left_to` call in `bisect::tips` (`src/bisect.rs`); drop it and the
//! runner that does not own the check runs it beside its live owner. The timeout test in
//! `bisect::left_to`; drop it and a check whose owner gave no answer within its timeout waits on
//! that owner. The liveness filter in `presence::live` (`src/presence.rs`); drop it and a check
//! waits on an owner whose presence reads stale.

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use support::{EXECUTOR, TestBox, check_files, passing_result, result_json};

/// A runner's weight for the post check `suite` in the environment `local`: the first eight
/// bytes of the SHA-256 of the check, its environment and the runner's name.
pub(crate) fn weight(runner: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("suite\0local\0{runner}"));
    u64::from_be_bytes(digest[..8].try_into().unwrap())
}

/// Two runner clones of one remote that serve one environment, and the post check `suite` with
/// `timeout_s`. Each runner writes its presence on the tests' 1 s cadence, so it reads live for
/// 33 s.
/// Returns the clone that owns `suite`, the clone that does not, and the file each run of
/// `suite` writes a line to.
fn two_runners(test_box: &TestBox, timeout_s: u64) -> (PathBuf, PathBuf, PathBuf) {
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
                "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = {timeout_s}\n"
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

/// Lands a change that writes `path` in `dir`.
pub(crate) fn land(test_box: &TestBox, repo: &Path, dir: &str, path: &str) {
    let started = test_box.bay_ok(repo, &["start", path, "--paths", dir]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
}

/// One runner pass with the reader's clock `skew_s` ahead: the post check it ran, or `""`.
pub(crate) fn ran_post(test_box: &TestBox, repo: &Path, skew_s: u64) -> String {
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

fn runs_of(runs: &Path) -> usize {
    std::fs::read_to_string(runs)
        .unwrap_or_default()
        .lines()
        .count()
}

#[test]
fn a_post_check_runs_on_its_owner_alone_while_the_owner_is_live() {
    let test_box = TestBox::new();
    let (owner, other, runs) = two_runners(&test_box, 600);

    land(&test_box, &owner, "src", "src/s0.rs");
    assert_eq!(ran_post(&test_box, &owner, 0), "suite");
    land(&test_box, &owner, "src", "src/s1.rs");
    assert_eq!(
        ran_post(&test_box, &other, 0),
        "",
        "a runner ran a post check its live owner answers"
    );
    assert_eq!(ran_post(&test_box, &owner, 0), "suite");
    assert_eq!(runs_of(&runs), 2, "two tips, one run each");

    // The owner stops: 300 s on, its presence reads stale, and the other runner runs the check.
    land(&test_box, &owner, "src", "src/s2.rs");
    assert_eq!(
        ran_post(&test_box, &other, 300),
        "suite",
        "a post check waited on an owner whose presence reads stale"
    );
    assert_eq!(runs_of(&runs), 3);
}

#[test]
fn a_post_check_its_owner_has_not_answered_within_its_timeout_runs_on_another() {
    let test_box = TestBox::new();
    let (owner, other, runs) = two_runners(&test_box, 5);

    land(&test_box, &owner, "src", "src/s0.rs");
    assert_eq!(ran_post(&test_box, &owner, 0), "suite");
    land(&test_box, &owner, "src", "src/s1.rs");
    // 5 s on, the owner still reads live but has given no answer within the check's 5 s.
    assert_eq!(
        ran_post(&test_box, &other, 5),
        "suite",
        "a post check waited past its timeout on an owner that gave no answer"
    );
    assert_eq!(runs_of(&runs), 2);
}

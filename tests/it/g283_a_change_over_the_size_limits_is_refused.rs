//! G283 (owner ruling 2026-10-21). A change over the size limits is refused.
//!
//! `[limits]` in `.bay/policy.toml`, judged at the accepted base, bounds one change:
//! `max_file_mib` (any one file it adds or changes), `max_files` (files it touches) and
//! `max_added_lines` (lines it adds; a binary file counts by its size alone). Each key is
//! optional, and an absent key is no limit. `bay land` refuses a change over one at submit, and
//! the executor at candidate build, both before any check runs: `CHANGE_TOO_LARGE` (blocked,
//! exit 10), naming each limit exceeded, its value and the largest offenders, with a plain
//! `say`. A change within the limits lands. A limit that is not a positive integer is
//! `POLICY_INVALID`.
//!
//! Guards to mutate: `crate::publish::too_large_at_submit`'s
//! `if exceeded.is_empty() {` (make it `if true {`: the submit cases land, or run the check);
//! `crate::publish::too_large`'s `if exceeded.is_empty() {` (make it `if true {`: the
//! executor case lands); and the comparisons in `crate::limits::exceeded`,
//! `bytes > over_bytes`, `count > max` and `total > max` (make each `false`: its case lands).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;
use std::fmt::Write as _;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestBox, UNCHECKED_POLICY, check_files, passing_result, result_json};

/// The limits of the seeded policy.
const LIMITS: &str = "\n[limits]\nmax_file_mib = 1\nmax_files = 3\nmax_added_lines = 50\n";

/// A repository whose trunk holds `src/a.rs`, one required check that logs each run to the
/// returned path, and [`LIMITS`].
fn seeded(test_box: &TestBox) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let runs = test_box.path().join("check-runs");
    let before = test_box.relay(&format!("echo run >> '{}'", runs.display()));
    let mut files = check_files(&before, &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(LIMITS);
        }
    }
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (repo, runs)
}

/// How many times the check ran.
fn runs(path: &Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |text| text.lines().count())
}

/// A new change on `src/` with `files` (path under the view, contents) written in its view.
fn change(test_box: &TestBox, repo: &Path, intent: &str, files: &[(String, Vec<u8>)]) -> PathBuf {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    for (path, bytes) in files {
        std::fs::write(view.join(path), bytes).unwrap();
    }
    view
}

/// `refused` is `CHANGE_TOO_LARGE` over `limit` alone, naming `paths` first among its offenders.
fn assert_too_large(refused: &Value, limit: &str, max: u64, paths: &[&str]) {
    assert_eq!(refused["decision"], "blocked", "{refused}");
    assert_eq!(refused["reason"], "CHANGE_TOO_LARGE", "{refused}");
    assert_eq!(refused["allowed"], json!(["read", "repair"]), "{refused}");
    assert_eq!(refused["retry_when"], "change_within_limits", "{refused}");
    let exceeded = refused["detail"]["exceeded"].as_array().unwrap();
    assert_eq!(exceeded.len(), 1, "only {limit} is exceeded: {refused}");
    assert_eq!(exceeded[0]["limit"], limit, "{refused}");
    assert_eq!(exceeded[0]["max"], max, "{refused}");
    let named: Vec<&str> = exceeded[0]["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect();
    assert_eq!(&named[..paths.len()], paths, "{refused}");
    let say = refused["say"].as_str().unwrap();
    assert!(
        say.starts_with("This change is too big to save here ("),
        "{say}"
    );
    assert!(say.contains("shared drive"), "{say}");
}

#[test]
fn a_change_over_each_limit_is_refused_before_any_check_and_one_within_lands() {
    let test_box = TestBox::new();
    let (repo, log) = seeded(&test_box);

    // One file over 1 MiB. It is binary, so it counts by its size alone, not by lines.
    let big = vec![0_u8; 1024 * 1024 + 512 * 1024];
    let view = change(
        &test_box,
        &repo,
        "CARD-1",
        &[("src/big.bin".to_owned(), big)],
    );
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_too_large(&refused, "max_file_mib", 1, &["src/big.bin"]);
    assert_eq!(
        refused["detail"]["exceeded"][0]["value"],
        1024 * 1024 + 512 * 1024
    );
    assert!(
        refused["say"]
            .as_str()
            .unwrap()
            .contains("src/big.bin is 1.5 MiB, over 1 MiB"),
        "{refused}"
    );

    // Four files, over three.
    let many: Vec<(String, Vec<u8>)> = (1..=4)
        .map(|n| {
            (
                format!("src/f{n}.rs"),
                format!("fn f{n}() {{}}\n").into_bytes(),
            )
        })
        .collect();
    let view = change(&test_box, &repo, "CARD-2", &many);
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_too_large(&refused, "max_files", 3, &["src/f1.rs", "src/f2.rs"]);
    assert_eq!(refused["detail"]["exceeded"][0]["value"], 4, "{refused}");

    // Sixty added lines, over fifty.
    let sixty = (1..=60).fold(String::new(), |mut text, n| {
        let _ = writeln!(text, "// line {n}");
        text
    });
    let view = change(
        &test_box,
        &repo,
        "CARD-3",
        &[("src/long.rs".to_owned(), sixty.into_bytes())],
    );
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_too_large(&refused, "max_added_lines", 50, &["src/long.rs"]);
    assert_eq!(refused["detail"]["exceeded"][0]["value"], 60, "{refused}");

    // No check ran for any of them.
    assert_eq!(runs(&log), 0, "a change over a limit is never checked");

    // Within every limit: checked once, and landed.
    let view = change(
        &test_box,
        &repo,
        "CARD-4",
        &[("src/small.rs".to_owned(), b"fn small() {}\n".to_vec())],
    );
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert_eq!(runs(&log), 1);
}

#[test]
fn the_executor_refuses_a_change_over_the_limits_at_candidate_build() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[(".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned())],
    );
    // Submitted under a policy with no limits: queued for the executor.
    let view = change(
        &test_box,
        &repo,
        "CARD-1",
        &[
            ("src/b.rs".to_owned(), b"fn b() {}\n".to_vec()),
            ("src/c.rs".to_owned(), b"fn c() {}\n".to_vec()),
        ],
    );
    let (code, queued) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");
    let id = queued["change"].as_str().unwrap().to_owned();

    // The policy at the shared copy then sets a limit the change is over.
    let other = test_box.clone_of(&remote, "other");
    let policy = "revision = 2\nunchecked = true\n\n[limits]\nmax_files = 1\n";
    std::fs::write(other.join(".bay/policy.toml"), policy).unwrap();
    test_box.git_ok(&other, &["commit", "-q", "-am", "limits"]);
    test_box.git_ok(&other, &["push", "-q", "origin", "main"]);
    let trunk = test_box.git_ok(&other, &["rev-parse", "HEAD"]);

    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    let one = &ran["detail"]["ran"][0];
    // The executor's summary of the run carries the decision, the reason and the detail.
    assert_eq!(one["decision"], "blocked", "{ran}");
    assert_eq!(one["reason"], "CHANGE_TOO_LARGE", "{ran}");
    let exceeded = &one["detail"]["exceeded"];
    assert_eq!(exceeded.as_array().map(Vec::len), Some(1), "{ran}");
    assert_eq!(exceeded[0]["limit"], "max_files", "{ran}");
    assert_eq!(exceeded[0]["value"], 2, "{ran}");
    assert_eq!(
        exceeded[0]["paths"],
        json!(["src/b.rs", "src/c.rs"]),
        "{ran}"
    );
    assert_eq!(one["detail"]["base"], trunk.as_str(), "{ran}");
    assert_eq!(one["detail"]["at"], "candidate", "{ran}");
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        trunk,
        "nothing landed"
    );
    let status = test_box.bay_ok(&executor, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["outcome"], "CHANGE_TOO_LARGE", "{status}");
    assert_eq!(op["state"], "blocked", "{status}");
}

#[test]
fn a_limit_that_is_not_a_positive_integer_is_policy_invalid() {
    for limits in [
        "max_files = 0",
        "max_files = -1",
        "max_file_mib = 1.5",
        "max_added_lines = \"10\"",
        "max_bytes = 10",
    ] {
        let test_box = TestBox::new();
        let repo = test_box.repo_any("repo");
        let policy = format!("{UNCHECKED_POLICY}\n[limits]\n{limits}\n");
        test_box.seed_with_check(
            &repo,
            &[("src/a.rs", "fn a() {}\n")],
            &[(".bay/policy.toml".to_owned(), policy)],
        );
        let view = change(
            &test_box,
            &repo,
            "CARD-1",
            &[("src/b.rs".to_owned(), b"fn b() {}\n".to_vec())],
        );
        let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
        assert_eq!(code, 10, "{limits}: {refused}");
        assert_eq!(refused["reason"], "POLICY_INVALID", "{limits}: {refused}");
    }
}

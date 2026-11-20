//! G811 (a fleet measurement, 2026-11-20: in 3.5 days on one repository 11 changes an agent's
//! own `bay merge --wait` claimed were blocked `CHECK_UNKNOWN` because the check adapter gave no
//! answer on the agent's machine, each waiting a median 700 s for a person to send it again
//! before a runner checked it). An own run whose check gives no answer on its machine hands the
//! operation to a live runner that serves the check's environment, once per machine: the runner
//! checks it again (unknown never passes) and lands it, with no new request; the own run's wait
//! goes on and reports the runner's outcome, saying a runner ran the check. With no live runner
//! that serves it, or for a check that failed, the own run blocks as before.
//!
//! The adapter gives no answer when its declared `PROBE` variable says `silent` (the agent's
//! machine) and passes otherwise (the runner's).
//!
//! Guard to mutate, in `src/publish.rs`: `handed_to_runners` answering `None` (the own run blocks
//! `CHECK_UNKNOWN`, and its wait reports that).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, check_files, passing_result};

/// A repository whose one check declares `PROBE`, gives no answer when it is `silent` and fails
/// when it is `fail`, the developer's machine named `dev-box`, and a runner's clone on
/// `runner-box`. With `runner_live`, the runner made one pass, so its presence is live. Returns
/// the repository, the remote and the runner's clone.
fn setup(test_box: &TestBox, runner_live: bool) -> (PathBuf, PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let pass = passing_result(1);
    let fail = pass.replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let files: Vec<(String, String)> = check_files(
        "[ \"$PROBE\" = silent ] && exit 0",
        &format!("if [ \"$PROBE\" = fail ]; then {fail}; else {pass}; fi"),
        1,
        60,
    )
    .into_iter()
    .map(|(path, text)| {
        if path == ".bay/policy.toml" {
            (path, format!("{text}env = [\"PROBE\"]\n"))
        } else {
            (path, text)
        }
    })
    .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    test_box.git_ok(&repo, &["config", "bay.machine", "dev-box"]);
    let runner = test_box.clone_of(&remote, "runner");
    test_box.git_ok(&runner, &["config", "bay.machine", "runner-box"]);
    test_box.git_ok(&runner, &["config", "bay.runnerName", "exec-r"]);
    if runner_live {
        let pass = test_box
            .bay(&runner)
            .args(["--json", "runner", "run", "--once", "--interval-s", "300"])
            .env("PROBE", "ok")
            .output()
            .unwrap();
        assert!(pass.status.success(), "{pass:?}");
    }
    (repo, remote, runner)
}

/// `bay merge --wait=180` of a change in a view of `repo`, run on the developer's machine with
/// `PROBE` set to `probe`, started in the background. Returns its process and the change.
fn own_run(test_box: &TestBox, repo: &Path, probe: &str) -> (std::process::Child, String) {
    let (id, view) = test_box.start_and_edit(repo);
    let child = test_box
        .bay_process(&view)
        .args(["--json", "merge", "--wait=180"])
        .env("PROBE", probe)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    (child, id)
}

/// The subjects of change `id`'s chain on `remote`, newest first.
fn chain(test_box: &TestBox, remote: &Path, id: &str) -> Vec<String> {
    test_box
        .git_bare(
            remote,
            &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

fn until(within: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(began.elapsed() < within, "{what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn result_of(child: std::process::Child) -> Value {
    let out = child.wait_with_output().unwrap();
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stdout)))
}

#[test]
fn an_own_run_whose_check_gives_no_answer_hands_it_to_a_live_runner() {
    let test_box = TestBox::new();
    // GIVEN a live runner that serves the check, and an own run on a machine where the check's
    // adapter gives no answer
    let (repo, remote, runner) = setup(&test_box, true);
    let (waiting, id) = own_run(&test_box, &repo, "silent");
    // WHEN the own run's check gives no answer
    until(
        Duration::from_secs(120),
        "the own run never handed the change to the runners",
        || {
            chain(&test_box, &remote, &id)
                .iter()
                .any(|subject| subject.contains("dev-box could not run"))
        },
    );
    // THEN the runner takes it, with no new request, and lands it
    let pass = test_box
        .bay(&runner)
        .args(["--json", "runner", "run", "--once"])
        .env("PROBE", "ok")
        .output()
        .unwrap();
    assert!(pass.status.success(), "{pass:?}");
    let result = result_of(waiting);
    assert_eq!(
        result["decision"],
        "merged",
        "{result}\nRUNNER {}\nCHAIN {:#?}",
        String::from_utf8_lossy(&pass.stdout),
        chain(&test_box, &remote, &id)
    );
    assert_eq!(
        result["detail"]["checked_elsewhere"]["machine"], "dev-box",
        "{result}"
    );
    assert!(
        result["say"]
            .as_str()
            .unwrap_or_default()
            .contains("A runner ran the check"),
        "{result}"
    );
    let subjects = chain(&test_box, &remote, &id);
    assert!(
        !subjects
            .iter()
            .any(|subject| subject.starts_with("blocked") || subject.starts_with("rearmed")),
        "{subjects:#?}"
    );
}

#[test]
fn without_a_live_runner_that_serves_it_the_own_run_blocks_as_before() {
    let test_box = TestBox::new();
    let (repo, _remote, _runner) = setup(&test_box, false);
    let (waiting, _id) = own_run(&test_box, &repo, "silent");
    let result = result_of(waiting);
    assert_eq!(result["reason"], "CHECK_UNKNOWN", "{result}");
}

#[test]
fn a_check_that_failed_on_this_machine_is_never_handed_back() {
    let test_box = TestBox::new();
    let (repo, _remote, _runner) = setup(&test_box, true);
    let (waiting, _id) = own_run(&test_box, &repo, "fail");
    let result = result_of(waiting);
    assert_eq!(result["reason"], "CHECK_FAILED", "{result}");
}

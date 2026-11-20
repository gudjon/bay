//! G816 (a fleet finding, 2026-11-20): after its machine handed an operation to the runners
//! (G811), a plain `bay merge --wait` watched the change with no deadline. When no runner took
//! it, or its repository was deleted, the wait never ended. The watch now ends at a queue head's
//! progress deadline and answers as `merge --wait=<s>` does (`queued`, the run continues). It
//! ends at once when the repository is gone.
//!
//! Guards to mutate, in `src/publish.rs`: `wait_land` watching with no deadline (a far
//! `until`), and `watch_until_done` going on when the git directory is gone.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, check_files, passing_result};

/// A repository whose one check gives no answer on the developer's machine (`dev-box`, where
/// `PROBE` is `silent`), with `config` as its `.bay/config.toml`, and a live runner on
/// `runner-box` that serves the check and never runs again. Returns the repository and the
/// remote.
fn setup(test_box: &TestBox, config: &str) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files: Vec<(String, String)> = check_files(
        "[ \"$PROBE\" = silent ] && exit 0",
        &passing_result(1),
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
    files.push((".bay/config.toml".to_owned(), config.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    test_box.git_ok(&repo, &["config", "bay.machine", "dev-box"]);
    let runner = test_box.clone_of(&remote, "runner");
    test_box.git_ok(&runner, &["config", "bay.machine", "runner-box"]);
    test_box.git_ok(&runner, &["config", "bay.runnerName", "exec-r"]);
    let pass = test_box
        .bay(&runner)
        .args(["--json", "runner", "run", "--once", "--interval-s", "300"])
        .env("PROBE", "ok")
        .output()
        .unwrap();
    assert!(pass.status.success(), "{pass:?}");
    (repo, remote)
}

/// A plain `bay merge --wait` of a change in a view of `repo`, run on the developer's machine,
/// started in the background. Returns its process, the change and its view.
fn plain_wait(test_box: &TestBox, repo: &Path) -> (Child, String, PathBuf) {
    let (id, view) = test_box.start_and_edit(repo);
    let child = test_box
        .bay_process(&view)
        .args(["--json", "merge", "--wait"])
        .env("PROBE", "silent")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    (child, id, view)
}

/// Whether change `id`'s chain on `remote` says the developer's machine handed it on.
fn handed(test_box: &TestBox, remote: &Path, id: &str) -> bool {
    test_box
        .git_bare(
            remote,
            &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
        )
        .lines()
        .any(|subject| subject.contains("dev-box could not run"))
}

/// The result `child` printed, once it ended within `within`. A wait still running then is
/// stopped, and the test fails.
fn ended_within(mut child: Child, within: Duration) -> Value {
    let began = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if began.elapsed() > within {
            let _ = child.kill();
            panic!("the wait did not end within {within:?}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let out = child.wait_with_output().unwrap();
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stdout)))
}

#[test]
fn a_handed_over_wait_that_no_runner_takes_ends_at_the_progress_deadline() {
    let test_box = TestBox::new();
    // GIVEN a progress deadline of three seconds: an execution budget of two, a publish window
    // of one
    let (repo, remote) = setup(
        &test_box,
        "[runner]\nexecution_budget_s = 2\npublish_window_s = 1\n",
    );
    // WHEN a plain wait hands its change to the runners, and no runner takes it
    let (waiting, id, _view) = plain_wait(&test_box, &repo);
    let result = ended_within(waiting, Duration::from_secs(90));
    // THEN the wait ends and answers as `merge --wait=<s>` does
    assert!(handed(&test_box, &remote, &id), "{result}");
    assert_eq!(result["decision"], "queued", "{result}");
    assert_eq!(result["state"], "running", "{result}");
    assert_eq!(result["waiting_for"], "runner", "{result}");
    assert_eq!(result["detail"]["the run continues"], true, "{result}");
    assert_eq!(result["detail"]["observation_timeout_s"], 3, "{result}");
}

#[test]
fn a_handed_over_wait_ends_when_its_repository_is_gone() {
    let test_box = TestBox::new();
    // GIVEN the default progress deadline, more than half an hour
    let (repo, remote) = setup(&test_box, "");
    let (waiting, id, view) = plain_wait(&test_box, &repo);
    let began = Instant::now();
    while !handed(&test_box, &remote, &id) {
        assert!(
            began.elapsed() < Duration::from_secs(120),
            "the wait never handed the change to the runners"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    // The hand-over is on the remote before the wait starts to watch it.
    std::thread::sleep(Duration::from_secs(2));
    // WHEN the change's view and its repository are deleted
    std::fs::remove_dir_all(&view).unwrap();
    std::fs::remove_dir_all(&repo).unwrap();
    // THEN the wait ends, and the run continues with the runners
    let result = ended_within(waiting, Duration::from_secs(60));
    assert_eq!(result["decision"], "queued", "{result}");
    assert_eq!(result["detail"]["the run continues"], true, "{result}");
}

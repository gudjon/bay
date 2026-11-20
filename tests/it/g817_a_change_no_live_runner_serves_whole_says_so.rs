//! G817 (a fleet finding, 2026-11-20: a change needed checks in two environments, each live
//! runner served one of them and none served both, and it waited 2.6 hours while its author was
//! told it was being checked, with nothing to do). A runner takes a change that waits for a
//! worker only when its box serves every environment the change waits for. When no live runner
//! serves them together, `bay status`, `bay merge --wait` and `bay merge --wait=<s>` say so,
//! name the environments, and say to tell the user (`served_together: false`).
//!
//! Guards to mutate, in `src/status.rs`: `served_together` always true; in `src/publish.rs`:
//! `unserved_whole` answering `None`, and `wait_for_worker` writing no `served_together`.

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// A repository whose policy has one check in `env-a` and one in `env-b`, on a developer box
/// that serves neither, with a live runner for `env-a` and one for `env-b`. Returns the
/// repository and the remote.
fn setup(test_box: &TestBox) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files: Vec<(String, String)> = check_files("true", &passing_result(1), 1, 60)
        .into_iter()
        .map(|(path, text)| {
            if path == ".bay/policy.toml" {
                let second = text
                    .split_once("[[check]]")
                    .unwrap()
                    .1
                    .replace("id = \"tests\"", "id = \"lint\"")
                    .replace("\"local\"", "\"env-b\"");
                let first = text.replace("\"local\"", "\"env-a\"");
                (path, format!("{first}\n[[check]]{second}"))
            } else {
                (path, text)
            }
        })
        .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    test_box.git_ok(
        &repo,
        &["config", "--add", "bay.checkEnvironment", "elsewhere"],
    );
    runner_pass(test_box, &remote, "a", &["env-a"]);
    runner_pass(test_box, &remote, "b", &["env-b"]);
    (repo, remote)
}

/// One pass of a runner named `exec-<name>` in its own clone of `remote`, on a box that serves
/// `environments`.
fn runner_pass(test_box: &TestBox, remote: &Path, name: &str, environments: &[&str]) -> Value {
    let runner = test_box.path().join(format!("runner-{name}"));
    if !runner.exists() {
        let runner = test_box.clone_of(remote, &format!("runner-{name}"));
        test_box.git_ok(&runner, &["config", "bay.machine", &format!("box-{name}")]);
        test_box.git_ok(
            &runner,
            &["config", "bay.runnerName", &format!("exec-{name}")],
        );
        for environment in environments {
            test_box.git_ok(
                &runner,
                &["config", "--add", "bay.checkEnvironment", environment],
            );
        }
    }
    let (code, ran) =
        result_json(
            test_box
                .bay(&runner)
                .args(["runner", "run", "--once", "--interval-s", "300"]),
        );
    assert_eq!(code, 0, "{ran}");
    ran
}

fn tells_the_user(result: &Value) -> bool {
    let say = result["say"].as_str().unwrap_or_default();
    say.contains("env-a and env-b together") && say.contains("tell the user")
}

#[test]
fn a_timed_wait_and_the_status_say_no_live_runner_serves_it_whole() {
    let test_box = TestBox::new();
    // GIVEN a live runner for each of the two environments, and none for both
    let (repo, _remote) = setup(&test_box);
    // WHEN a change sent with `bay merge --wait=15` waits for a worker for both (the wait reads
    // what its run waits for when the window ends, and under load the run takes seconds to start)
    let (id, view) = test_box.start_and_edit(&repo);
    let (_, sent) = result_json(test_box.bay(&view).args(["merge", "--wait=15"]));
    // THEN the wait and the status name them, say no live runner serves them together, and say
    // to tell the user
    assert_eq!(sent["reason"], "CHECK_UNAVAILABLE", "{sent}");
    assert!(tells_the_user(&sent), "{sent}");
    let (code, status) = result_json(test_box.bay(&view).args(["status", &id]));
    assert_eq!(code, 0, "{status}");
    let operation = &status["detail"]["changes"][0]["operation"];
    assert_eq!(operation["waiting_for"], "worker", "{status}");
    assert_eq!(
        operation["environments"],
        serde_json::json!(["env-a", "env-b"]),
        "{status}"
    );
    assert_eq!(operation["served_together"], false, "{status}");
    let say = status["say"].as_str().unwrap_or_default();
    assert!(
        say.contains("serves env-a and env-b together") && say.contains("tell the user"),
        "{status}"
    );
    assert!(!say.contains("being checked"), "{status}");
}

#[test]
fn a_plain_wait_says_no_live_runner_serves_it_whole() {
    let test_box = TestBox::new();
    let (repo, _remote) = setup(&test_box);
    let (_, view) = test_box.start_and_edit(&repo);
    let (_, sent) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(sent["reason"], "CHECK_UNAVAILABLE", "{sent}");
    assert_eq!(sent["detail"]["served_together"], false, "{sent}");
    assert!(tells_the_user(&sent), "{sent}");
}

#[test]
fn a_runner_whose_box_serves_both_merges_it() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    let (id, view) = test_box.start_and_edit(&repo);
    let (_, sent) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(sent["reason"], "CHECK_UNAVAILABLE", "{sent}");
    // WHEN the runners that serve one each pass, and then one whose box serves both
    runner_pass(&test_box, &remote, "a", &["env-a"]);
    runner_pass(&test_box, &remote, "b", &["env-b"]);
    let ran = runner_pass(&test_box, &remote, "ab", &["env-a", "env-b"]);
    // THEN only the last takes it, and it merges
    let (_, status) = result_json(test_box.bay(&view).args(["status", &id]));
    assert_eq!(
        status["detail"]["changes"][0]["state"], "merged",
        "{status}\nRAN {ran}"
    );
}

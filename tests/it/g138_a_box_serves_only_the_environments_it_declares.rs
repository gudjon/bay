//! G138. A box serves only the environments it declares.
//!
//! A box declares the policy environments its executor serves in Git config,
//! `bay.checkEnvironment`, globally or for one clone, never in `.bay/config.toml`. An executor
//! whose box does not serve a required environment records `CHECK_UNAVAILABLE` on the
//! operation, runs nothing and stops renewing its lease, so an executor whose box serves it
//! claims the operation after the lease, with a higher epoch, and lands it once. While it waits,
//! `bay status` says it waits for a worker and names the environment. Doctor's `environments`
//! row names what the box serves and whether that was declared. A box that declares nothing
//! serves every environment, as before the declaration existed, so an upgrade stops no executor
//! (`DESIGN` § 7, `INTERFACES` § 7, § 8; rulings-open-questions.md Q4).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json, row};

/// A trunk whose one required check runs in `env-b` and appends a line to `runs` each time it
/// runs, and a change submitted for an executor. Returns (change id, repo, remote, runs).
fn submitted(test_box: &TestBox) -> (String, PathBuf, PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    let files: Vec<(String, String)> = check_files(
        &test_box.relay(&format!("echo \"$tree\" >> '{}'", runs.display())),
        &passing_result(1),
        1,
        60,
    )
    .into_iter()
    .map(|(path, text)| (path, text.replace("\"local\"", "\"env-b\"")))
    .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{result}");
    (id, repo, remote, runs)
}

/// An executor clone of `remote` named `name`, whose clone declares `environment`.
fn executor(test_box: &TestBox, remote: &Path, name: &str, environment: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    test_box.git_ok(&clone, &["config", "bay.checkEnvironment", environment]);
    clone
}

fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path).map_or(0, |text| text.lines().count())
}

/// E1 declares `env-a` and E2 declares `env-b`, under a policy that needs `env-b`. E1 records
/// `CHECK_UNAVAILABLE`, runs nothing, and keeps making passes that never renew the lease; E2
/// claims the operation after one lease with epoch 2 and lands it once.
#[test]
fn a_box_serves_only_the_environments_it_declares() {
    let test_box = TestBox::new();
    let (id, repo, remote, runs) = submitted(&test_box);
    let e1 = executor(&test_box, &remote, "exec-a", "env-a");
    let e2 = executor(&test_box, &remote, "exec-b", "env-b");

    let first = test_box.bay_ok(&e1, &["runner", "run", "--once"]);
    let result = &first["detail"]["ran"][0];
    assert_eq!(result["decision"], "queued", "{first}");
    assert_eq!(result["reason"], "CHECK_UNAVAILABLE", "{first}");
    assert_eq!(
        result["detail"]["environments"],
        serde_json::json!(["env-b"]),
        "{first}"
    );
    assert_eq!(line_count(&runs), 0, "E1 ran a check it does not serve");

    let status = test_box.bay_ok(&repo, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["waiting_for"], "worker", "{status}");
    assert_eq!(op["environments"], serde_json::json!(["env-b"]), "{status}");
    assert_eq!(op["owner"], "exec-a", "{status}");

    // E1 keeps passing over the operation while E2 watches its lease: it never renews it.
    let mut looping = test_box
        .bay_process(&e1)
        .args(["--json", "runner", "run", "--interval-s", "1"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let second = test_box.bay_ok(&e2, &["runner", "run", "--once"]);
    let _ = looping.kill();
    let _ = looping.wait();
    let result = &second["detail"]["ran"][0];
    assert_eq!(result["decision"], "merged", "{second}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    assert_eq!(line_count(&runs), 1, "one check ran, on E2");
    let log = test_box.git_bare(
        &remote,
        &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
    );
    assert!(log.contains("epoch 2 by exec-b"), "{log}");
    assert!(
        !log.contains("epoch 3"),
        "E1 claimed again while it does not serve env-b: {log}"
    );
}

/// A box that declares nothing serves every environment, as it did before the declaration
/// existed: its executor runs the `env-b` check and lands the change, and doctor names the
/// list it serves as the default.
#[test]
fn a_box_that_declares_nothing_serves_every_environment() {
    let test_box = TestBox::new();
    let (id, _, remote, runs) = submitted(&test_box);
    let clone = test_box.clone_of(&remote, "executor");

    let (code, result) = result_json(test_box.bay(&clone).args(["doctor", "--role", "runner"]));
    let environments = row(&result, "environments");
    assert_eq!(environments["source"], "default", "{result}");
    assert_eq!(environments["declared"], serde_json::json!([]), "{result}");
    assert_eq!(
        environments["serves"],
        serde_json::json!(["env-b"]),
        "{result}"
    );
    assert_eq!(environments["state"], "green", "{result}");
    assert_eq!(code, 0, "{result}");

    let ran = test_box.bay_ok(&clone, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
    // It ran the `env-b` check: at least once, since a lease that runs out under a loaded gate
    // box lets the operation be taken over and checked again. The cases that must not run
    // assert zero runs.
    assert!(line_count(&runs) >= 1, "the box never ran the env-b check");
}

/// Doctor's `environments` row: a declared list is green and says so; a declaration that names
/// no environment (an empty value, as a box under maintenance sets) is red for the executor
/// role with `CHECK_UNAVAILABLE` and the command that fixes it, and informational on an agent
/// view.
#[test]
fn doctor_names_the_environments_this_box_serves() {
    let test_box = TestBox::new();
    let (_, _, remote, _) = submitted(&test_box);
    let clone = executor(&test_box, &remote, "exec-b", "env-b");
    let (code, result) = result_json(test_box.bay(&clone).args(["doctor", "--role", "runner"]));
    let environments = row(&result, "environments");
    assert_eq!(environments["source"], "declared", "{result}");
    assert_eq!(
        environments["declared"],
        serde_json::json!(["env-b"]),
        "{result}"
    );
    assert_eq!(
        environments["serves"],
        serde_json::json!(["env-b"]),
        "{result}"
    );
    assert_eq!(environments["state"], "green", "{result}");
    assert_eq!(code, 0, "{result}");

    test_box.git_ok(&clone, &["config", "bay.checkEnvironment", ""]);
    let (code, result) = result_json(test_box.bay(&clone).args(["doctor", "--role", "runner"]));
    let environments = row(&result, "environments");
    assert_eq!(environments["source"], "declared", "{result}");
    assert_eq!(environments["serves"], serde_json::json!([]), "{result}");
    assert_eq!(environments["state"], "red", "{result}");
    assert!(
        environments["recovery"]
            .as_str()
            .is_some_and(|recovery| recovery.contains("bay.checkEnvironment")),
        "{result}"
    );
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "CHECK_UNAVAILABLE", "{result}");

    let (code, result) = result_json(test_box.bay(&clone).arg("doctor"));
    let environments = row(&result, "environments");
    assert_eq!(environments["state"], "informational", "{result}");
    assert_eq!(environments["required"], false, "{result}");
    assert_eq!(code, 0, "{result}");
}

/// A repository never claims what a box can do: `.bay/config.toml` has no key for a check
/// environment, so a config that tries is refused whole and nothing starts under it.
#[test]
fn the_repository_config_cannot_declare_an_environment() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\nlease_s = 2\nheartbeat_s = 1\ncheck_environment = [\"env-b\"]\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .args(["start", "CARD-1", "--paths", "src/"]),
    );
    assert_ne!(code, 0, "{result}");
    assert!(result.to_string().contains("check_environment"), "{result}");
}

/// A queue head released because its box serves none of its environments (a live queue: a
/// second change waits behind it) is taken by a box that serves them, never held by the box
/// that recorded the wait (a fleet finding, 2026-10-26: a change sat three hours `running`
/// under a box with no route to the environment it needed).
#[test]
fn a_head_its_box_cannot_serve_is_taken_by_one_that_can_with_a_queue_live() {
    let test_box = TestBox::new();
    let (first, repo, remote, _runs) = submitted(&test_box);
    let (second, view) = test_box.start_and_edit(&repo);
    // Its own edit, so it is never work the first change already put on main.
    std::fs::remove_file(view.join("src/new.rs")).unwrap();
    std::fs::write(view.join("src/second.rs"), "fn second() {}\n").unwrap();
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let e1 = executor(&test_box, &remote, "exec-a", "env-a");
    let e2 = executor(&test_box, &remote, "exec-b", "env-b");
    let waited = test_box.bay_ok(&e1, &["runner", "run", "--once"]);
    assert!(waited.to_string().contains("CHECK_UNAVAILABLE"), "{waited}");
    for _ in 0..3 {
        let ran = test_box.bay_ok(&e2, &["runner", "run", "--once"]);
        if test_box.trunk_change_ids(&remote).len() == 2 {
            break;
        }
        eprintln!("{ran}");
    }
    let mut landed = test_box.trunk_change_ids(&remote);
    landed.sort();
    let mut both = vec![first, second];
    both.sort();
    assert_eq!(landed, both, "the box that serves env-b merged both");
}

//! G138. A box serves only the environments it declares.
//!
//! A box declares the policy environments its executor serves in Git config,
//! `bay.checkEnvironment`, globally or for one clone, never in `.bay/config.toml`. An executor
//! whose box does not serve a required environment records `CHECK_UNAVAILABLE` on the
//! operation, runs nothing and stops renewing its lease, so an executor whose box serves it
//! claims the operation at once, with a higher epoch, and lands it once (G425: a waiting
//! operation holds no lease against a box that serves it). A wait recorded under an older
//! policy keeps only the environments the policy at trunk still requires (G431). A change that
//! raises the policy is routed by its own new checks too (G579). While it waits,
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
    submitted_with(test_box, None)
}

/// [`submitted`], with `.bay/config.toml` set to `config` (the test seed keeps a lease the
/// config names, and gives every other one two seconds).
fn submitted_with(test_box: &TestBox, config: Option<&str>) -> (String, PathBuf, PathBuf, PathBuf) {
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
    .chain(config.map(|text| (".bay/config.toml".to_owned(), text.to_owned())))
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
/// claims the operation with epoch 2 and lands it once.
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

    // E1 keeps passing over the operation and never renews its lease.
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

/// A fleet finding (2026-10-26): while a live runner whose box serves a head's environments is
/// there, a box that does not serve them never claims the head. Before, it claimed it, found no
/// worker and released the place, which sent the change to the back of the queue.
#[test]
fn a_box_that_cannot_serve_the_head_leaves_it_to_a_live_one_that_can() {
    let test_box = TestBox::new();
    let (first, repo, remote, _runs) = submitted(&test_box);
    let (second, view) = test_box.start_and_edit(&repo);
    std::fs::remove_file(view.join("src/new.rs")).unwrap();
    std::fs::write(view.join("src/second.rs"), "fn second() {}\n").unwrap();
    let e1 = executor(&test_box, &remote, "exec-a", "env-a");
    let e2 = executor(&test_box, &remote, "exec-b", "env-b");
    // E2's presence is fresh: it made a pass before the second change queued behind the first.
    let _ = test_box
        .bay_process(&e2)
        .args(["--json", "runner", "run", "--once"])
        .env("BAY_TEST_PAUSE_AT", "before-claim")
        .env("BAY_TEST_PAUSE_FILE", test_box.path().join("never"))
        .env("BAY_TEST_PAUSE_TIMEOUT_S", "2")
        .output();
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let waited = test_box.bay_ok(&e1, &["runner", "run", "--once"]);
    let log = test_box.git_bare(
        &remote,
        &["log", "--format=%s", &format!("refs/bay/changes/{first}")],
    );
    assert!(
        !log.contains("by exec-a"),
        "the box that cannot serve env-b claimed the head: {log}\n{waited}"
    );
    let released = test_box.git_bare(
        &remote,
        &[
            "for-each-ref",
            "--format=%(contents:subject)",
            "refs/bay/queue-done/",
        ],
    );
    assert!(!released.contains("unavailable"), "{released}");
    for _ in 0..3 {
        test_box.bay_ok(&e2, &["runner", "run", "--once"]);
        if test_box.trunk_change_ids(&remote).len() == 2 {
            break;
        }
    }
    let mut landed = test_box.trunk_change_ids(&remote);
    landed.sort();
    let mut both = vec![first, second];
    both.sort();
    assert_eq!(landed, both, "the box that serves env-b merged both");
}

/// G425 (a fleet finding, 2026-10-28): an operation its holder left waiting for a worker holds
/// no lease against a box that serves it. Before, that box watched a full lease, ten minutes
/// on one fleet, while it ran nothing else, and only then took it over.
#[test]
fn a_box_that_serves_a_waiting_operation_takes_it_at_once() {
    let test_box = TestBox::new();
    let (id, _repo, remote, runs) =
        submitted_with(&test_box, Some("[runner]\nlease_s = 60\nheartbeat_s = 1\n"));
    let e1 = executor(&test_box, &remote, "exec-a", "env-a");
    let e2 = executor(&test_box, &remote, "exec-b", "env-b");
    let first = test_box.bay_ok(&e1, &["runner", "run", "--once"]);
    assert_eq!(
        first["detail"]["ran"][0]["reason"], "CHECK_UNAVAILABLE",
        "{first}"
    );
    let started = std::time::Instant::now();
    let second = test_box.bay_ok(&e2, &["runner", "run", "--once"]);
    let took = started.elapsed();
    assert_eq!(second["detail"]["ran"][0]["decision"], "merged", "{second}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
    assert_eq!(line_count(&runs), 1, "one check ran, on E2");
    assert!(
        took < std::time::Duration::from_secs(30),
        "E2 watched {took:?} before it took the waiting operation; the lease is 60 s"
    );
}

/// G431 (a fleet finding, 2026-10-28): a wait recorded under an older policy names an
/// environment the policy at trunk no longer requires of the change. Only an environment still
/// required keeps the wait, so a box that serves what the policy now needs takes the change;
/// before, every box that did not serve the recorded one passed it over for hours.
#[test]
fn a_wait_for_an_environment_the_policy_no_longer_needs_is_over() {
    let test_box = TestBox::new();
    let (id, repo, remote, runs) = submitted(&test_box);
    let e1 = executor(&test_box, &remote, "exec-a", "env-a");
    let first = test_box.bay_ok(&e1, &["runner", "run", "--once"]);
    assert_eq!(
        first["detail"]["ran"][0]["reason"], "CHECK_UNAVAILABLE",
        "{first}"
    );
    // The policy moves the check to env-a, which E1 serves.
    test_box.git_ok(&repo, &["checkout", "-q", "main"]);
    test_box.git_ok(&repo, &["pull", "-q", "--ff-only", "origin", "main"]);
    let policy = std::fs::read_to_string(repo.join(".bay/policy.toml")).unwrap();
    std::fs::write(
        repo.join(".bay/policy.toml"),
        policy.replace("\"env-b\"", "\"env-a\""),
    )
    .unwrap();
    test_box.git_ok(&repo, &["commit", "-q", "-am", "the check runs in env-a"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "HEAD:main"]);
    let second = test_box.bay_ok(&e1, &["runner", "run", "--once"]);
    assert!(
        test_box.trunk_change_ids(&remote).contains(&id),
        "a wait for env-b outlived the policy that asked for it: {second}"
    );
    assert_eq!(line_count(&runs), 1, "one check ran, on E1");
}

/// A trunk whose one check runs in `env-a`, and a change that raises the policy and adds a
/// check in `env-b`, submitted for an executor. Returns (change id, remote, runs).
fn policy_change_submitted(test_box: &TestBox) -> (String, PathBuf, PathBuf) {
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
    .map(|(path, text)| (path, text.replace("\"local\"", "\"env-a\"")))
    .collect();
    let policy = files
        .iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| text.clone())
        .unwrap();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let started = test_box.bay_ok(&repo, &["start", "CARD-G579", "--paths", ".bay/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let check = &policy[policy.find("[[check]]").unwrap()..];
    let own = format!(
        "{}\n{}",
        policy.replace("revision = 1", "revision = 2"),
        check
            .replace("\"tests\"", "\"more\"")
            .replace("\"env-a\"", "\"env-b\"")
    );
    std::fs::write(view.join(".bay/policy.toml"), own).unwrap();
    let (code, result) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{result}");
    (id, remote, runs)
}

/// G579 (a fleet finding, 2026-10-30): a change to the policy is checked by its own new
/// policy too, so the environments its own checks run in route it. A box that serves only what
/// the policy at trunk needs leaves it to a live box that serves them all, and runs nothing.
/// Before, every box claimed it, ran the base's check, found no worker for the new one and sent
/// the change to the back.
#[test]
fn a_change_to_the_policy_is_routed_by_its_own_checks_too() {
    let test_box = TestBox::new();
    let (id, remote, runs) = policy_change_submitted(&test_box);
    let e1 = executor(&test_box, &remote, "exec-a", "env-a");
    let e2 = executor(&test_box, &remote, "exec-b", "env-a");
    test_box.git_ok(&e2, &["config", "--add", "bay.checkEnvironment", "env-b"]);
    // E2's presence is fresh: it made a pass that stopped before its claim.
    let _ = test_box
        .bay_process(&e2)
        .args(["--json", "runner", "run", "--once"])
        .env("BAY_TEST_PAUSE_AT", "before-claim")
        .env("BAY_TEST_PAUSE_FILE", test_box.path().join("never"))
        .env("BAY_TEST_PAUSE_TIMEOUT_S", "2")
        .output();
    let passed = test_box.bay_ok(&e1, &["runner", "run", "--once"]);
    let log = test_box.git_bare(
        &remote,
        &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
    );
    assert!(
        !log.contains("by exec-a"),
        "the box that cannot serve env-b claimed the policy change: {log}\n{passed}"
    );
    assert_eq!(line_count(&runs), 0, "E1 ran no check");
    for _ in 0..3 {
        test_box.bay_ok(&e2, &["runner", "run", "--once"]);
        if test_box.trunk_change_ids(&remote).contains(&id) {
            break;
        }
    }
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id], "E2 landed it");
}

/// G579: with no live box that serves its own new check, a box that recorded the wait for it
/// passes the policy change over, and claims and checks it no more.
#[test]
fn a_wait_for_a_policy_changes_own_check_holds() {
    let test_box = TestBox::new();
    let (id, remote, runs) = policy_change_submitted(&test_box);
    let e1 = executor(&test_box, &remote, "exec-a", "env-a");
    let first = test_box.bay_ok(&e1, &["runner", "run", "--once"]);
    assert_eq!(
        first["detail"]["ran"][0]["reason"], "CHECK_UNAVAILABLE",
        "{first}"
    );
    let ran = line_count(&runs);
    let second = test_box.bay_ok(&e1, &["runner", "run", "--once"]);
    assert_eq!(
        line_count(&runs),
        ran,
        "E1 checked the change again: {second}"
    );
    let log = test_box.git_bare(
        &remote,
        &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
    );
    assert_eq!(log.matches("by exec-a").count(), 1, "{log}");
}

//! G9. A missed notice never hides a landing (`DESIGN` § 6, § 9; `INTERFACES` § 2, § 4, § 5;
//! ruling R6).
//!
//! An outcome the executor reaches after the agent left is written as a `bay.event/1` record
//! on the executor's log before anyone is told. The notify adapter is then called once with
//! the event on stdin; exit 0 means delivered, and nothing is retried or written back into the
//! record. Here the adapter fails every call. `bay watch --since <cursor>` still shows the
//! landing (read from trunk), the event (read from the log) and the invalidation (derived
//! through the named inputs), and a watch from the cursor it returns shows nothing new.
//! Neither the notice nor the watch changes what the change observed.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json, write_executable};

const INPUT: &str = "contract:api";

/// The provider: the version is the contents of `api/version`; `src/` consumes it.
const PROVIDER: &str = "#!/bin/sh\ncat >/dev/null\nv=$(cat api/version) || exit 1\nprintf '{\"version\":\"%s\",\"consumers\":[{\"path\":\"src/\",\"kind\":\"executable\"}]}' \"$v\"\n";

/// A trunk at `v17` whose notify adapter appends each event it is given to `<root>/notified`
/// and exits `notify_exit`, with the check `result`. Returns the box, the work repository, the
/// remote and the notice file.
fn trunk(notify_exit: u8, result: &str) -> (TestBox, PathBuf, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let notified = test_box.path().join("notified");
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    write_executable(&repo.join("scripts/api-version"), PROVIDER);
    write_executable(
        &repo.join("scripts/bay-notify"),
        &format!(
            "#!/bin/sh\ncat >> '{0}'\necho >> '{0}'\necho \"${{SSH_AUTH_SOCK:-unset}}\" >> '{0}.env'\ngit --git-dir='{1}' rev-parse main >> '{0}.trunk'\nexit {notify_exit}\n",
            notified.display(),
            remote.display()
        ),
    );
    let mut files = check_files("", result, 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            *text = format!(
                "{text}\n[[input]]\nname = \"{INPUT}\"\nprovider = \"scripts/api-version\"\nadapter = [\"scripts/api-version\"]\npath = [\"/usr/bin\", \"/bin\"]\n"
            );
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[notify]\ncommand = \"scripts/bay-notify\"\n".to_owned(),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/client.rs", "uses v17\n"), ("api/version", "v17\n")],
        &files,
    );
    (test_box, repo, remote, notified)
}

fn start(test_box: &TestBox, repo: &Path, args: &[&str]) -> (String, PathBuf) {
    let started = test_box.bay_ok(repo, args);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    (id, view)
}

fn watch(test_box: &TestBox, view: &Path, since: Option<&str>) -> serde_json::Value {
    let mut args = vec!["watch"];
    if let Some(cursor) = since {
        args.extend(["--since", cursor]);
    }
    let watched = test_box.bay_ok(view, &args);
    assert_eq!(watched["decision"], "ok", "{watched}");
    assert!(watched["detail"]["cursor"].is_string(), "{watched}");
    watched
}

/// The events the notify adapter was given, one JSON document per call.
fn notices(notified: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(notified)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn observed(test_box: &TestBox, repo: &Path, id: &str) -> serde_json::Value {
    let record = test_box.git_ok(
        repo,
        &[
            "cat-file",
            "blob",
            &format!("refs/bay/changes/{id}:change.json"),
        ],
    );
    serde_json::from_str::<serde_json::Value>(&record).unwrap()["observed_versions"].clone()
}

#[test]
fn a_missed_notice_never_hides_a_landing_or_an_invalidation() {
    let (test_box, repo, remote, notified) = trunk(1, &passing_result(1));

    // Y reads contract:api at v17, takes a cursor, submits and leaves.
    let (y, view_y) = start(
        &test_box,
        &repo,
        &["start", "CARD-Y", "--input", INPUT, "--paths", "src/"],
    );
    std::fs::write(view_y.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    let first = watch(&test_box, &view_y, None);
    assert_eq!(first["change"], y.as_str(), "{first}");
    assert_eq!(
        first["detail"]["invalidations"],
        serde_json::json!([]),
        "{first}"
    );
    let cursor = first["detail"]["cursor"].as_str().unwrap().to_owned();
    let (code, queued) = result_json(test_box.bay(&view_y).arg("land"));
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");

    // X moves the contract to v18 and lands.
    let (x, view_x) = start(&test_box, &repo, &["start", "CARD-X", "--paths", "api/"]);
    std::fs::write(view_x.join("api/version"), "v18\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view_x).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");

    // The executor, on another clone, refuses Y after its agent left: an event, and one notice
    // that fails.
    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["change"], y.as_str(), "{ran}");
    assert_eq!(
        ran["detail"]["ran"][0]["reason"], "INPUT_UNOBSERVED",
        "{ran}"
    );
    let told = notices(&notified);
    assert_eq!(told.len(), 1, "one call per event: {told:?}");
    assert_eq!(told[0]["schema"], "bay.event/1", "{told:?}");
    assert_eq!(told[0]["kind"], "refused", "{told:?}");
    assert_eq!(told[0]["change"], y.as_str(), "{told:?}");
    assert_eq!(told[0]["reason"], "INPUT_UNOBSERVED", "{told:?}");
    assert!(
        told[0]["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("evt_")),
        "{told:?}"
    );

    // The notice was lost. Y's watch from its cursor still shows all three.
    let seen = watch(&test_box, &view_y, Some(&cursor));
    let detail = &seen["detail"];
    let landings = detail["landings"].as_array().unwrap();
    assert!(
        landings
            .iter()
            .any(|landing| landing["change"] == x.as_str()),
        "the landing is read from trunk: {seen}"
    );
    let events = detail["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{seen}");
    assert_eq!(events[0]["id"], told[0]["id"], "{seen}");
    assert_eq!(events[0]["reason"], "INPUT_UNOBSERVED", "{seen}");
    let invalidations = detail["invalidations"].as_array().unwrap();
    assert_eq!(invalidations.len(), 1, "{seen}");
    assert_eq!(invalidations[0]["input"], INPUT, "{seen}");
    assert_eq!(invalidations[0]["reason"], "INPUT_UNOBSERVED", "{seen}");
    assert_eq!(invalidations[0]["observed"], "v17", "{seen}");
    assert_eq!(invalidations[0]["required"], "v18", "{seen}");

    // From the cursor it returned, nothing is new.
    let next = detail["cursor"].as_str().unwrap();
    assert_ne!(next, cursor, "{seen}");
    let again = watch(&test_box, &view_y, Some(next));
    assert_eq!(
        again["detail"]["landings"],
        serde_json::json!([]),
        "{again}"
    );
    assert_eq!(again["detail"]["events"], serde_json::json!([]), "{again}");
    assert_eq!(
        again["detail"]["invalidations"],
        serde_json::json!([]),
        "{again}"
    );
    assert_eq!(again["detail"]["cursor"], next, "{again}");

    // A failed notice is never retried, and nothing changed what Y observed.
    test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(notices(&notified).len(), 1, "a notice is never retried");
    assert_eq!(
        observed(&test_box, &repo, &y),
        serde_json::json!({INPUT: "v17"})
    );
}

#[test]
fn a_check_that_fails_after_the_agent_left_is_an_event() {
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let (test_box, repo, remote, notified) = trunk(0, &failing);
    let (y, view_y) = start(&test_box, &repo, &["start", "CARD-Y", "--paths", "src/"]);
    std::fs::write(view_y.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    let cursor = watch(&test_box, &view_y, None)["detail"]["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, _) = result_json(test_box.bay(&view_y).arg("land"));
    assert_eq!(code, 0);
    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["reason"], "CHECK_FAILED", "{ran}");
    let told = notices(&notified);
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["kind"], "check_failed", "{told:?}");
    assert_eq!(told[0]["actor"]["name"], "agent", "{told:?}");
    let seen = watch(&test_box, &view_y, Some(&cursor));
    assert_eq!(seen["detail"]["events"][0]["id"], told[0]["id"], "{seen}");
    assert_eq!(
        seen["detail"]["events"][0]["recurrence_key"],
        format!("{y}/CHECK_FAILED/tests"),
        "{seen}"
    );
    assert_eq!(seen["detail"]["landings"], serde_json::json!([]), "{seen}");
}

#[test]
fn a_malformed_cursor_is_a_usage_error() {
    let (test_box, repo, _remote, _notified) = trunk(0, &passing_result(1));
    let out = test_box
        .bay(&repo)
        .args(["watch", "--since", "not a cursor"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
}

/// The events on every executor log of the remote, oldest first per log.
fn logged_events(test_box: &TestBox, remote: &Path) -> Vec<serde_json::Value> {
    let logs = test_box.git_bare(
        remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/log/"],
    );
    let mut events = Vec::new();
    for log in logs.lines() {
        for commit in test_box
            .git_bare(remote, &["rev-list", "--reverse", log])
            .lines()
        {
            let text = test_box.git_bare(remote, &["show", &format!("{commit}:record.json")]);
            let record: serde_json::Value = serde_json::from_str(&text).unwrap();
            if record["schema"] == "bay.event/1" {
                events.push(record);
            }
        }
    }
    events
}

#[test]
fn an_executor_that_dies_before_the_event_leaves_it_to_the_next_pass() {
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let (test_box, repo, remote, notified) = trunk(0, &failing);
    let (y, view_y) = start(&test_box, &repo, &["start", "CARD-Y", "--paths", "src/"]);
    std::fs::write(view_y.join("src/feature.rs"), "fn feature() {}\n").unwrap();
    let cursor = watch(&test_box, &view_y, None)["detail"]["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, _) = result_json(test_box.bay(&view_y).arg("land"));
    assert_eq!(code, 0);

    // The executor blocks the operation on its chain, then dies before its log holds the event.
    let executor = test_box.clone_of(&remote, "executor");
    let died = test_box
        .bay(&executor)
        .args(["executor", "run", "--once"])
        .env("BAY_TEST_KILL_AT", "before-outcome-event")
        .output()
        .unwrap();
    assert_eq!(died.status.code(), Some(137), "{died:?}");
    assert_eq!(
        logged_events(&test_box, &remote),
        Vec::<serde_json::Value>::new()
    );
    assert_eq!(notices(&notified), Vec::<serde_json::Value>::new());

    // The next pass writes the event the log lacks, and tells the adapter once.
    test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    let events = logged_events(&test_box, &remote);
    assert_eq!(
        events.len(),
        1,
        "the next pass writes the lost event: {events:?}"
    );
    assert_eq!(events[0]["kind"], "check_failed", "{events:?}");
    assert_eq!(events[0]["change"], y.as_str(), "{events:?}");
    let told = notices(&notified);
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["id"], events[0]["id"], "{told:?}");
    let seen = watch(&test_box, &view_y, Some(&cursor));
    assert_eq!(seen["detail"]["events"][0]["id"], events[0]["id"], "{seen}");

    // Once held, the event is never written or told again.
    test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(logged_events(&test_box, &remote).len(), 1);
    assert_eq!(notices(&notified).len(), 1, "a notice is never retried");
}

#[test]
fn a_policy_changed_outside_bay_reaches_every_change_and_a_watch_outside_a_view() {
    let (test_box, repo, remote, notified) = trunk(0, &passing_result(1));
    let outside = watch(&test_box, &repo, None)["detail"]["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_y, view_y) = start(&test_box, &repo, &["start", "CARD-Y", "--paths", "src/"]);
    let cursor = watch(&test_box, &view_y, None)["detail"]["cursor"]
        .as_str()
        .unwrap()
        .to_owned();

    // The policy is edited straight on the remote, then another change, Z, lands under it.
    let forge = test_box.clone_of(&remote, "forge");
    let policy = std::fs::read_to_string(forge.join(".bay/policy.toml")).unwrap();
    std::fs::write(
        forge.join(".bay/policy.toml"),
        format!("{policy}\n# edited outside bay\n"),
    )
    .unwrap();
    test_box.git_ok(
        &forge,
        &["commit", "-q", "-am", "policy edited outside bay"],
    );
    test_box.git_ok(&forge, &["push", "-q", "origin", "main"]);
    let (z, view_z) = start(&test_box, &repo, &["start", "CARD-Z", "--paths", "src/"]);
    std::fs::write(view_z.join("src/z.rs"), "fn z() {}\n").unwrap();
    let (code, landed) = result_json(
        test_box
            .bay(&view_z)
            .args(["land", "--wait"])
            .env("SSH_AUTH_SOCK", "/nonexistent/agent.sock"),
    );
    assert_eq!(code, 0, "{landed}");
    let told = notices(&notified);
    assert_eq!(told.len(), 1, "{told:?}");
    // The adapter never gets the executor's environment, and is told only once Z's operation
    // reached its outcome, never while it holds its lease.
    let env = std::fs::read_to_string(notified.with_extension("env")).unwrap();
    assert_eq!(env.trim(), "unset", "{env}");
    let told_at = std::fs::read_to_string(notified.with_extension("trunk")).unwrap();
    assert_eq!(
        told_at.trim(),
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        "{landed}"
    );
    assert_eq!(told[0]["kind"], "policy_changed_outside_bay", "{told:?}");
    assert_eq!(told[0]["change"], z.as_str(), "{told:?}");

    // The policy is an input of every change: Y's watch lists the event Z's operation wrote.
    let seen = watch(&test_box, &view_y, Some(&cursor));
    let ids: Vec<&serde_json::Value> = seen["detail"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| &event["id"])
        .collect();
    assert_eq!(ids, vec![&told[0]["id"]], "{seen}");

    // Outside a view, every event after the cursor is listed.
    let all = watch(&test_box, &repo, Some(&outside));
    assert!(
        all["detail"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["id"] == told[0]["id"]),
        "{all}"
    );
}

//! G67. A trunk with no live executor is stated (fleet-rollout-analysis F6).
//!
//! Each `bay runner run` pass publishes the executor's presence, `refs/bay/executor/<name>`:
//! one parentless commit whose `presence.json` names the executor, its machine, its bay and Git,
//! the environments the box serves, the time of the pass, the operation it drives first and the
//! queue depth it saw. `bay status` fetches it with the fleet refs, `bay status --fleet` lists
//! it with its age and `stale`, and `bay doctor --role executor` shows the box's own. With
//! operations queued and no fresh presence, `bay status` warns `no_live_executor`: never a
//! refusal, and never a first fetch of an old presence read as a live executor.

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json, row, unchecked_files};

/// A repository with a trunk and a check, its remote, and a clone the executor runs in.
fn seeded() -> (TestBox, PathBuf, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let executor = test_box.clone_of(&remote, "executor");
    (test_box, repo, remote, executor)
}

/// One `bay runner run --once` in `dir`, which must succeed.
fn once(test_box: &TestBox, dir: &Path) -> Value {
    let (code, result) = result_json(test_box.bay(dir).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{result}");
    result
}

/// The presence record the remote holds for executor `name`, and its commit.
fn presence(test_box: &TestBox, remote: &Path, name: &str) -> (String, Value) {
    let reference = format!("refs/bay/executor/{name}");
    let commit = test_box.git_bare(remote, &["rev-parse", &reference]);
    let text = test_box.git_bare(
        remote,
        &["cat-file", "blob", &format!("{commit}:presence.json")],
    );
    (commit, serde_json::from_str(&text).unwrap())
}

/// `bay status` in `dir`, with the reader's clock moved `skew_s` seconds on.
fn status(test_box: &TestBox, dir: &Path, skew_s: u64) -> Value {
    let (code, result) = result_json(
        test_box
            .bay(dir)
            .arg("status")
            .env("BAY_TEST_PRESENCE_CLOCK_SKEW_S", skew_s.to_string()),
    );
    assert_eq!(code, 0, "{result}");
    result
}

/// The `no_live_executor` warning of a status result, if it carries one.
fn no_live_executor(result: &Value) -> Option<&Value> {
    result["detail"]["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == "no_live_runner")
}

#[test]
fn an_executor_pass_publishes_its_presence_and_replaces_the_last_one() {
    let (test_box, repo, remote, executor) = seeded();
    let ran = once(&test_box, &executor);
    assert_eq!(ran["detail"]["presence"]["pushed"], true, "{ran}");
    let (first, record) = presence(&test_box, &remote, "local");
    assert_eq!(record["schema"], "bay.executor/1", "{record}");
    assert_eq!(record["executor"], "local", "{record}");
    assert_eq!(record["bay"], bay::version::BAY, "{record}");
    assert!(record["machine"].is_string(), "{record}");
    assert!(record["git"].is_string(), "{record}");
    assert!(record["at"].is_string(), "{record}");
    assert_eq!(record["environments"]["source"], "default", "{record}");
    assert_eq!(record["holding"], Value::Null, "{record}");
    assert_eq!(record["queue_depth"], 0, "{record}");
    // No pid: the machine names the box.
    assert!(record.get("pid").is_none(), "{record}");

    // With an operation queued, the next pass names it as the one it drives first, and its
    // presence replaces the last one: one parentless commit, never a growing chain.
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let op_id = queued["op_id"].as_str().unwrap().to_owned();
    once(&test_box, &executor);
    let (second, record) = presence(&test_box, &remote, "local");
    assert_ne!(first, second);
    assert_eq!(record["holding"], op_id.as_str(), "{record}");
    assert_eq!(
        test_box.git_bare(&remote, &["rev-list", "--count", "refs/bay/executor/local"]),
        "1"
    );
}

#[test]
fn the_fleet_map_lists_each_executor_with_its_version_and_environments() {
    let (test_box, repo, _remote, executor) = seeded();
    test_box.git_ok(&executor, &["config", "bay.machine", "box-exec"]);
    test_box.git_ok(
        &executor,
        &["config", "--add", "bay.checkEnvironment", "linux-x86_64"],
    );
    once(&test_box, &executor);

    status(&test_box, &repo, 0);
    let (code, fleet) = result_json(test_box.bay(&repo).args(["status", "--fleet"]));
    assert_eq!(code, 0, "{fleet}");
    let executors = fleet["detail"]["runners"]
        .as_array()
        .unwrap_or_else(|| panic!("no executors: {fleet}"));
    assert_eq!(executors.len(), 1, "{fleet}");
    let seen = &executors[0];
    assert_eq!(seen["runner"], "local", "{fleet}");
    assert_eq!(seen["machine"], "box-exec", "{fleet}");
    assert_eq!(seen["bay"], bay::version::BAY, "{fleet}");
    assert!(seen["git"].is_string(), "{fleet}");
    assert_eq!(seen["environments"]["source"], "declared", "{fleet}");
    assert_eq!(
        seen["environments"]["declared"],
        serde_json::json!(["linux-x86_64"]),
        "{fleet}"
    );
    assert_eq!(seen["readable"], true, "{fleet}");
    assert_eq!(seen["stale"], false, "{fleet}");
    assert!(
        seen["age_s"].is_u64() && seen["stale_after_s"].is_u64(),
        "{fleet}"
    );
    assert!(seen["last_pass"].is_string(), "{fleet}");

    // The box's own presence, in doctor.
    let (_, doctor) = result_json(
        test_box
            .bay(&executor)
            .args(["doctor", "--role", "executor"]),
    );
    let own = row(&doctor, "presence");
    assert_eq!(own["state"], "green", "{doctor}");
    assert_eq!(own["runner"], "local", "{doctor}");
    assert_eq!(own["bay"], bay::version::BAY, "{doctor}");
    assert_eq!(own["last_push"]["pushed"], true, "{doctor}");
}

#[test]
fn queued_work_with_only_a_stale_presence_says_no_live_executor() {
    let (test_box, repo, _remote, executor) = seeded();
    once(&test_box, &executor);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");

    // The executor's pass a moment ago is a live executor: no warning.
    let fresh = status(&test_box, &view, 0);
    assert!(no_live_executor(&fresh).is_none(), "{fresh}");

    // An hour on, the same presence is stale: the queued work is told nobody is executing it.
    let later = status(&test_box, &view, 3600);
    let warning = no_live_executor(&later).unwrap_or_else(|| panic!("no warning: {later}"));
    assert!(warning["queued"].as_u64() >= Some(1), "{later}");
    assert!(warning["seen_s"].as_u64() >= Some(3600), "{later}");
    assert!(
        warning["runner"]
            .as_str()
            .is_some_and(|text| text.starts_with("none seen for ")),
        "{later}"
    );
    assert_eq!(warning["runners"][0]["runner"], "local", "{later}");
    assert_eq!(warning["runners"][0]["stale"], true, "{later}");
    // A warning, never a refusal.
    assert_eq!(later["decision"], "ok", "{later}");
}

#[test]
fn queued_work_and_no_executor_ever_says_none_seen() {
    let (test_box, repo, _remote, _executor) = seeded();
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let result = status(&test_box, &view, 0);
    let warning = no_live_executor(&result).unwrap_or_else(|| panic!("no warning: {result}"));
    assert_eq!(warning["runner"], "none seen", "{result}");
    assert_eq!(warning["seen_s"], Value::Null, "{result}");
    assert_eq!(warning["runners"], serde_json::json!([]), "{result}");

    // Nothing queued, nothing to warn of.
    let (idle_box, idle_repo, _idle_remote, _idle_executor) = seeded();
    let idle = status(&idle_box, &idle_repo, 0);
    assert!(no_live_executor(&idle).is_none(), "{idle}");
}

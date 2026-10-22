//! G307. An outcome someone must act on wakes them once.
//!
//! When an executor pass records an outcome its author must act on, bay asks the configured
//! `[wake] command` once for it, as it asks once for a talk: a `bay.wake/1` request with
//! `cause: "outcome"`, the change, the operation, the decision, the reason code, and the actor
//! and session the change record names. The adapter's answer goes on the outcome event in the
//! executor's log as `wake: {result, reason}`, so `bay watch` shows it. A block the executor
//! reached wakes its author once, and a later pass over the same outcome never wakes again. A
//! landing the executor reached wakes its author once too. An outcome the author's own
//! `bay land --wait` reached returns to that session, so nothing wakes it. Without a `[wake]`
//! table nothing is run, and the event records `not_attempted`.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/publish.rs` line 1129, `if !reason.transient() {` around the blocked event's wake: make
//!   it `if false {` and the blocked case hears no wake.
//! - `src/publish.rs` line 1030, `let _ = landed_wake(&ctx, op_id, &result);` in `execute`: drop
//!   it and the landed case hears no wake; move the wake into `submit_land` and the
//!   `land --wait` case hears one.
//! - `src/talk/wake.rs` line 138, `"cause": "outcome"` in `outcome_request`: make it `"talk"` and
//!   the blocked case fails.
//! - `src/executor.rs` line 501, `runs`, with `src/publish.rs` line 1119, the held-event check in
//!   `outcome_event`: run blocked operations again and drop the check, and the second pass wakes
//!   a second time.
//! - `src/talk/wake.rs` line 103, the `not_attempted` answer with no `[wake] command`: return
//!   `failed()` instead and the case without a `[wake]` table fails.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{TestBox, check_files, passing_result, result_json, write_executable};

/// A trunk whose check answers `result`, with a wake adapter in `scripts/bay-wake` that appends
/// each request it is given to `<root>/woken` and answers `woken`. With `wake`, the config names
/// it in `[wake] command`; without, the script is there and nothing names it. Returns the box,
/// the work repository, the remote and the request file.
fn trunk(result: &str, wake: bool) -> (TestBox, PathBuf, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let woken = test_box.path().join("woken");
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    write_executable(
        &repo.join("scripts/bay-wake"),
        &format!(
            "#!/bin/sh\ncat >> '{}'\nprintf '{{\"schema\":\"bay.wake-result/1\",\"result\":\"woken\",\"reason\":null}}'\n",
            woken.display()
        ),
    );
    let mut files = check_files("", result, 1, 60);
    if wake {
        files.push((
            ".bay/config.toml".to_owned(),
            "[wake]\ncommand = \"scripts/bay-wake\"\n".to_owned(),
        ));
    }
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (test_box, repo, remote, woken)
}

fn failing() -> String {
    passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"")
}

/// The requests the wake adapter was given, one JSON document per call.
fn requests(woken: &Path) -> Vec<Value> {
    std::fs::read_to_string(woken)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The actor the change record of `id` names.
fn actor(test_box: &TestBox, repo: &Path, id: &str) -> Value {
    let record = test_box.git_ok(
        repo,
        &[
            "cat-file",
            "blob",
            &format!("refs/bay/changes/{id}:change.json"),
        ],
    );
    serde_json::from_str::<Value>(&record).unwrap()["actor"].clone()
}

/// The event of `kind` about change `id` that `bay watch` lists from `dir`.
fn event(test_box: &TestBox, dir: &Path, id: &str, kind: &str) -> Value {
    let watched = test_box.bay_ok(dir, &["watch"]);
    watched["detail"]["events"]
        .as_array()
        .and_then(|events| {
            events
                .iter()
                .find(|event| event["change"] == id && event["kind"] == kind)
        })
        .cloned()
        .unwrap_or_else(|| panic!("no {kind} event for {id}: {watched}"))
}

/// Submits a change and leaves; an executor on another clone runs one pass. Returns the change
/// id, the executor clone, the pass, and the change's view (where `bay watch` lists every event
/// of the change: outside a view, a watch with no cursor starts from now).
fn submitted_and_run(
    test_box: &TestBox,
    repo: &Path,
    remote: &Path,
) -> (String, PathBuf, Value, PathBuf) {
    let (id, view) = test_box.start_and_edit(repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");
    let executor = test_box.clone_of(remote, "executor");
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    (id, executor, ran, view)
}

#[test]
fn a_block_the_executor_reached_wakes_its_author_once() {
    let (test_box, repo, remote, woken) = trunk(&failing(), true);
    let (id, executor, ran, view) = submitted_and_run(&test_box, &repo, &remote);
    assert_eq!(ran["detail"]["ran"][0]["reason"], "CHECK_FAILED", "{ran}");

    let asked = requests(&woken);
    assert_eq!(asked.len(), 1, "one wake for one outcome: {asked:?}");
    let request = &asked[0];
    assert_eq!(request["schema"], "bay.wake/1", "{request}");
    assert_eq!(request["op"], "wake", "{request}");
    assert_eq!(request["cause"], "outcome", "{request}");
    assert_eq!(request["change"], id.as_str(), "{request}");
    assert_eq!(request["op_id"], format!("{id}.1"), "{request}");
    assert_eq!(request["decision"], "blocked", "{request}");
    assert_eq!(request["reason"], "CHECK_FAILED", "{request}");
    assert_eq!(request["urgent"], false, "{request}");
    let author = actor(&test_box, &repo, &id);
    assert_eq!(request["actor"]["name"], author["name"], "{request}");
    assert_eq!(request["actor"]["session"], author["session"], "{request}");
    assert!(request.get("talk").is_none(), "{request}");

    // The answer is on the outcome event, and `bay watch` shows it.
    let blocked = event(&test_box, &view, &id, "check_failed");
    assert_eq!(
        blocked["wake"],
        json!({"result": "woken", "reason": null}),
        "{blocked}"
    );

    // A second pass over the same outcome wakes no one.
    let again = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(again["detail"]["ran"], json!([]), "{again}");
    assert_eq!(requests(&woken).len(), 1, "a wake is asked once");
}

#[test]
fn a_landing_the_executor_reached_wakes_its_author_once() {
    let (test_box, repo, remote, woken) = trunk(&passing_result(1), true);
    let (id, executor, ran, view) = submitted_and_run(&test_box, &repo, &remote);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "landed", "{ran}");

    let asked = requests(&woken);
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0]["cause"], "outcome", "{asked:?}");
    assert_eq!(asked[0]["decision"], "landed", "{asked:?}");
    assert_eq!(asked[0]["reason"], Value::Null, "{asked:?}");
    assert_eq!(asked[0]["change"], id.as_str(), "{asked:?}");
    let author = actor(&test_box, &repo, &id);
    assert_eq!(asked[0]["actor"]["session"], author["session"], "{asked:?}");

    let wake = event(&test_box, &view, &id, "wake");
    assert_eq!(
        wake["wake"],
        json!({"result": "woken", "reason": null}),
        "{wake}"
    );

    let again = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(again["detail"]["ran"], json!([]), "{again}");
    assert_eq!(requests(&woken).len(), 1, "a wake is asked once");
}

#[test]
fn a_landing_the_author_waited_on_wakes_nothing() {
    let (test_box, repo, remote, woken) = trunk(&passing_result(1), true);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert_eq!(
        requests(&woken),
        Vec::<Value>::new(),
        "the session got its outcome"
    );

    // An executor pass afterwards has nothing to wake for either.
    let executor = test_box.clone_of(&remote, "executor");
    test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    assert_eq!(requests(&woken), Vec::<Value>::new());
}

#[test]
fn without_a_wake_table_nothing_is_run() {
    let (test_box, repo, remote, woken) = trunk(&failing(), false);
    let (id, _, ran, view) = submitted_and_run(&test_box, &repo, &remote);
    assert_eq!(ran["detail"]["ran"][0]["reason"], "CHECK_FAILED", "{ran}");
    assert!(!woken.exists(), "no adapter ran");
    let blocked = event(&test_box, &view, &id, "check_failed");
    assert_eq!(
        blocked["wake"],
        json!({"result": "not_attempted", "reason": "NO_WAKE_ADAPTER"}),
        "{blocked}"
    );
}

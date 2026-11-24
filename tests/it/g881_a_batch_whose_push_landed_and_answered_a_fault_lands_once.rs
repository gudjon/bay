//! G881 (a fleet finding, 2026-11-24: the host took a batch of eight with its records and answered
//! its push with a fault; the head read its chain again, found the batch landed, and yielded "main
//! moved" over its own landing, so its record read admitted on a change main held): a push the
//! host took can answer a fault. Main holding the batch's tip is its landing: each member is merged
//! in that pass, and no record goes on a landing. A single landing whose push answered a fault is
//! already found on main (`after_refused_push`); its case is here beside the batch's.
//!
//! Guard to mutate: in `src/publish.rs` `land_batch`, answer a refused batch push as refused whether
//! or not main holds its tip (the head yields over its landing, and its members are given back).
//!
//! G886 (a fleet measurement, 2026-11-24: 3 of 5 runners' own logs had forked so, at a batch's
//! landing): the records the host took with the push stand, and the runner's log is the remote's
//! again by the pass's end, each terminal record on it once. Guard to mutate: in `src/log.rs`
//! `append_riding`, drop the call of `reconcile_own`.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;

use support::queued::{Check, Held, host, ran_for};
use support::{PRODUCER, TestBox, result_json};

const CONFIG: &str = "[runner]\nlease_s = 450\nheartbeat_s = 120\nbatch = { min = 2, max = 8, max_failure_rate = 1.0 }\n";

/// The change record of `id` on `remote`.
fn chain(test_box: &TestBox, remote: &Path, id: &str) -> Value {
    serde_json::from_str(&test_box.git_bare(
        remote,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    ))
    .unwrap()
}

/// The subjects of the records on `id`'s chain on `remote`, newest first.
fn chain_subjects(test_box: &TestBox, remote: &Path, id: &str) -> Vec<String> {
    test_box
        .git_bare(
            remote,
            &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Makes the first push that moves main land, then answer a fault, as a host does whose answer
/// is lost after it took the push (an HTTP 502); returns the file that push leaves.
fn answer_a_fault_once(held: &Held) -> PathBuf {
    let answered = held.test_box.path().join("answered");
    held.test_box.wrap_git(&format!(
        "case \" $* \" in\n  *\" push \"*\":refs/heads/main \"*)\n    if [ ! -e '{answered}' ]; then\n      \"$real\" \"$@\" || exit $?\n      : > '{answered}'\n      echo 'error: RPC failed; HTTP 502 curl 22 The requested URL returned error: 502' >&2\n      echo 'fatal: the remote end hung up unexpectedly' >&2\n      exit 128\n    fi\n    ;;\nesac",
        answered = answered.display()
    ));
    answered
}

/// Each of `ids` is merged in `pass` and on main, recorded landed with its operation published,
/// and no record that gives it back or yields it went on its landing.
fn landed_once(held: &Held, pass: &Value, ids: &[String]) {
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    for id in ids {
        assert_eq!(ran_for(pass, id)["decision"], "merged", "{id}: {pass}");
        assert!(on_main.contains(id), "{id} is not on main: {on_main:?}");
        let record = chain(&held.test_box, &held.remote, id);
        assert_eq!(record["state"], "landed", "{id}: {record}");
        let last = record["operations"].as_array().unwrap().last().unwrap();
        assert_eq!(last["state"], "published", "{id}: {record}");
        let subjects = chain_subjects(&held.test_box, &held.remote, id);
        assert!(
            !subjects
                .iter()
                .any(|subject| subject.starts_with("yielded ") || subject.starts_with("released ")),
            "{id}: {subjects:#?}"
        );
    }
}

/// The runner's log at `exec` is the remote's, and each of `ids` has its terminal record on it once.
fn log_joined(held: &Held, exec: &Path, ids: &[String]) {
    let tips = |listed: String| listed.lines().map(str::to_owned).collect::<Vec<_>>();
    let format = "--format=%(refname) %(objectname)";
    let here = tips(
        held.test_box
            .git_ok(exec, &["for-each-ref", format, "refs/bay/log/"]),
    );
    let there = tips(
        held.test_box
            .git_bare(&held.remote, &["for-each-ref", format, "refs/bay/log/"]),
    );
    assert_eq!(here, there, "the runner's log and the remote's are apart");
    let subjects = held.test_box.git_bare(
        &held.remote,
        &["log", "--format=%s", "--glob=refs/bay/log/"],
    );
    for id in ids {
        let record = format!("terminal record {id}.1");
        let count = subjects
            .lines()
            .filter(|subject| *subject == record)
            .count();
        assert_eq!(count, 1, "{record}: {subjects}");
    }
}

fn held() -> Held {
    host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    })
}

#[test]
fn a_batch_whose_push_landed_with_its_records_and_answered_a_fault_is_merged_in_that_pass() {
    let held = held();
    let exec = held.executor("exec", PRODUCER);
    let ids: Vec<String> = ["b1", "b2", "b3"]
        .into_iter()
        .map(|name| held.queue(name, "src"))
        .collect();

    // GIVEN a host that takes the batch's push, its records with it, and answers a fault
    let answered = answer_a_fault_once(&held);

    // WHEN a pass lands the batch
    let (code, pass) = result_json(held.test_box.bay(&exec).args(["runner", "run", "--once"]));

    // THEN each member is merged in that pass, in the batch, and no record went on a landing
    assert!(answered.exists(), "no push moved main: {pass}");
    assert_eq!(code, 0, "{pass}");
    landed_once(&held, &pass, &ids);
    for id in &ids {
        assert!(
            !ran_for(&pass, id)["detail"]["batch"].is_null(),
            "not in a batch: {pass}"
        );
    }
    log_joined(&held, &exec, &ids);
}

#[test]
fn a_batch_whose_push_without_its_records_landed_and_answered_a_fault_is_merged_in_that_pass() {
    let held = held();
    let exec = held.executor("exec", PRODUCER);
    let ids: Vec<String> = ["c1", "c2"]
        .into_iter()
        .map(|name| held.queue(name, "src"))
        .collect();

    // GIVEN a host that takes the batch's push, its records kept apart, and answers a fault
    let answered = answer_a_fault_once(&held);

    // WHEN a pass lands the batch
    let (code, pass) = result_json(
        held.test_box
            .bay(&exec)
            .env("BAY_TEST_LAND_APART", "1")
            .args(["runner", "run", "--once"]),
    );

    // THEN each member is merged in that pass, each landing derived from main
    assert!(answered.exists(), "no push moved main: {pass}");
    assert_eq!(code, 0, "{pass}");
    landed_once(&held, &pass, &ids);
}

#[test]
fn a_landing_whose_push_landed_and_answered_a_fault_is_merged_in_that_pass() {
    let held = held();
    let exec = held.executor("exec", PRODUCER);
    let ids = vec![held.queue("d1", "src")];

    // GIVEN a host that takes the landing's push and answers a fault
    let answered = answer_a_fault_once(&held);

    // WHEN a pass lands the change
    let (code, pass) = result_json(held.test_box.bay(&exec).args(["runner", "run", "--once"]));

    // THEN it is merged in that pass, and no record went on its landing
    assert!(answered.exists(), "no push moved main: {pass}");
    assert_eq!(code, 0, "{pass}");
    landed_once(&held, &pass, &ids);
    log_joined(&held, &exec, &ids);
}

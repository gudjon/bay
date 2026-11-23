//! G868 (a fleet measurement, 2026-11-23: each landing's wake made two fetches of the logs and a
//! push of this executor's log after the publishing push, about 8 s of a runner's time a landing
//! on a hosted remote). After this process's own push landed an operation, no log can hold its
//! wake yet, so none is fetched to look for one, and the wake goes on this box's log only. The
//! next push of the log carries it: the next landing's, the one after the pass's first heads,
//! or one after the pass.
//!
//! Guards to mutate: in `src/publish.rs` `landed_wake`, set `ours` to `false` (two fetches of
//! the logs and a push of the log a landing); in `src/executor.rs` `run`, drop the push of the
//! log after a pass (a repair's wake never leaves the runner's box).

#![cfg(unix)]

use std::path::Path;

use crate::support;

use support::PRODUCER;
use support::queued::{Check, host, ran_for};
use support::result_json;

/// The wake adapter: every request answers woken.
const WAKE: &str = "#!/bin/sh\ncat >/dev/null\nprintf '{\"schema\":\"bay.wake-result/1\",\"result\":\"woken\",\"reason\":null}'\n";

const CONFIG: &str =
    "[plugin.wake]\ncommand = \"scripts/bay-wake\"\n\n[runner]\nlease_s = 450\nheartbeat_s = 120\n";

/// Each Git call one pass made, its arguments read from its trace, in order.
fn traced_calls(trace: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let call: serde_json::Value = serde_json::from_str(line).unwrap();
            let args = call["args"].as_array()?;
            Some(
                args.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            )
        })
        .collect()
}

/// The calls after the pass's last publishing push (the one that names a landed marker).
fn after_publishing(calls: &[Vec<String>]) -> &[Vec<String>] {
    let at = calls
        .iter()
        .rposition(|args| {
            args.iter().any(|arg| arg == "push")
                && args.iter().any(|arg| arg.contains("refs/bay/landed/"))
        })
        .unwrap_or_else(|| panic!("no publishing push: {calls:?}"));
    &calls[at + 1..]
}

/// Whether `args` is a fetch of the executors' logs.
fn fetches_logs(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "fetch") && args.iter().any(|arg| arg.contains("refs/bay/log/"))
}

/// Whether `args` is a push of this executor's log and nothing else.
fn pushes_log_alone(args: &[String]) -> bool {
    let Some(at) = args.iter().position(|arg| arg == "origin") else {
        return false;
    };
    let refs = &args[at + 1..];
    args.iter().any(|arg| arg == "push")
        && !refs.is_empty()
        && refs.iter().all(|spec| spec.starts_with("refs/bay/log/"))
}

/// How many wakes of `op_id` the remote's executor logs hold: each log commit holds its record
/// in `record.json`.
fn wakes_on_remote(remote: &Path, op_id: &str) -> usize {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg(format!("--git-dir={}", remote.display()))
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["log", "--format=%H %s", "--glob=refs/bay/log/"])
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, subject)| subject.starts_with("event wake "))
        .filter(|(commit, _)| {
            let record: serde_json::Value = serde_json::from_str(&git(&[
                "cat-file",
                "blob",
                &format!("{commit}:record.json"),
            ]))
            .unwrap();
            record["op_id"] == op_id
        })
        .count()
}

#[test]
fn a_landings_wake_rides_the_next_push_of_the_log() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n"), ("scripts/bay-wake", WAKE)],
        hold_first: false,
        fails: "false",
    });
    let exec = held.executor("exec", PRODUCER);
    // A first pass on its own, so the measured ones start from a runner that has run.
    let first = held.queue("first", "src");
    assert_eq!(ran_for(&held.pass(&exec), &first)["decision"], "merged");
    let traced = |name: &str| {
        let trace = held.test_box.path().join(name);
        let mut pass = held.test_box.bay(&exec);
        pass.env("BAY_TEST_GIT_TRACE", &trace);
        let (code, ran) = result_json(pass.args(["runner", "run", "--once"]));
        assert_eq!(code, 0, "{ran}");
        (traced_calls(&trace), ran)
    };

    // GIVEN a wake adapter and one change waiting WHEN a pass lands it
    let one = held.queue("one", "src");
    let (calls, ran) = traced("one.jsonl");
    assert_eq!(ran_for(&ran, &one)["decision"], "merged", "{ran}");
    // THEN no log is fetched after the publishing push, the log goes once, and the remote holds
    // the wake by the pass's end
    let after = after_publishing(&calls);
    let fetched: Vec<_> = after.iter().filter(|args| fetches_logs(args)).collect();
    assert!(
        fetched.is_empty(),
        "the logs fetched for a wake: {fetched:?}"
    );
    let alone: Vec<_> = after.iter().filter(|args| pushes_log_alone(args)).collect();
    assert!(
        alone.len() <= 1,
        "{} pushes of the log: {alone:?}",
        alone.len()
    );
    assert_eq!(wakes_on_remote(&held.remote, &format!("{one}.1")), 1);

    // GIVEN two changes waiting WHEN one pass lands both
    let ids: Vec<String> = ["two", "three"]
        .into_iter()
        .map(|name| held.queue(name, "src"))
        .collect();
    let (calls, ran) = traced("two.jsonl");
    for id in &ids {
        assert_eq!(ran_for(&ran, id)["decision"], "merged", "{ran}");
    }
    // THEN no fetch reads the logs for their wakes, one push of the log alone at most carries
    // both, and the remote holds each wake once
    let fetched: Vec<_> = calls.iter().filter(|args| fetches_logs(args)).collect();
    let before: Vec<_> = traced_calls(&held.test_box.path().join("one.jsonl"))
        .into_iter()
        .filter(|args| fetches_logs(args))
        .collect();
    assert!(
        fetched.len() <= before.len(),
        "a second landing fetched the logs again: {fetched:?}"
    );
    let alone: Vec<_> = calls.iter().filter(|args| pushes_log_alone(args)).collect();
    assert!(
        alone.len() <= 1,
        "{} pushes of the log: {alone:?}",
        alone.len()
    );
    for id in &ids {
        assert_eq!(wakes_on_remote(&held.remote, &format!("{id}.1")), 1, "{id}");
    }

    // GIVEN a repair of a landed change, which a pass visits after its push of the log, and no
    // change in the line WHEN a pass lands the repair
    let fixes = ids[0].as_str();
    let started = held.test_box.bay_ok(
        &held.repo,
        &["start", "four", "--paths", "src/", "--fixes", fixes],
    );
    let repair = started["change"].as_str().unwrap().to_owned();
    let view = Path::new(started["detail"]["view"].as_str().unwrap()).to_owned();
    std::fs::write(view.join("src/four.rs"), "fn four() {}\n").unwrap();
    let (code, submitted) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    let (_, ran) = traced("four.jsonl");
    assert_eq!(ran_for(&ran, &repair)["decision"], "merged", "{ran}");
    // THEN the push of the log after the pass carries its wake
    assert_eq!(wakes_on_remote(&held.remote, &format!("{repair}.1")), 1);
}

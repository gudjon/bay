//! G680 (BACKLOG 92, a fleet finding, 2026-11-08: a runner's merge spent about 23 s of its 64 s
//! median in 20 fetches; merges after a pass's first made 15 to 24, as each head reused what
//! the pass read since the landing before it, and then, by G438, read it all again before its
//! claim). A pass that lands three changes makes at most 22 fetches: after a landing the queue
//! is read once for the conflict notes and the abandoned-head check, the next head reads afresh
//! once, never twice, and a claim that waited for no one keeps the main its visit read.
//!
//! G681 (BACKLOG 92, the same reading: 8 pushes a merge, about 3 s each). Its evidence notes go
//! in the chain's push that names the candidate checked, atomically, and are not read back; an
//! executor's log append pushes on its own tip and fetches only when the push is refused. So
//! the pass makes at most 17 pushes and 2 listings.
//!
//! Read from the trace a test build writes when `BAY_TEST_GIT_TRACE` names a file (G171).
//!
//! Guards to mutate: in `src/executor.rs` `pass`, drop the `forget_reads` after a landing (each
//! later head reads twice); in `run_op`, read the queue with `Queue::read` (it is fetched again
//! for the abandoned-head check); in `src/publish.rs` `trunk_after_claim`, drop the
//! `claim_waited` test (main is fetched again after each claim); in `persist_attempt`, push the
//! evidence on its own again (a push more a landing); in `src/log.rs` `append`, fetch before
//! the write (a fetch more a log record).

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::queued::{Check, Trust, host_under};
use support::{PRODUCER, check_files, passing_result, result_json};

/// The calls of Git subcommand `sub` one pass made, read from its trace.
fn calls(trace: &std::path::Path, sub: &str) -> Vec<String> {
    std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let call: serde_json::Value = serde_json::from_str(line).unwrap();
            let args: Vec<&str> = call["args"]
                .as_array()?
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect();
            args.contains(&sub).then(|| args.join(" "))
        })
        .collect()
}

#[test]
fn a_pass_of_three_landings_fetches_within_its_budget() {
    let policy = check_files("x", &passing_result(1), 1, 120)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| text)
        .unwrap();
    let held = host_under(
        &Check {
            config: "[runner]\nlease_s = 30\nheartbeat_s = 30\n",
            policy: Some(&policy),
            seed: &[("src/a.rs", "fn a() {}\n")],
            hold_first: false,
            fails: "false",
        },
        Trust::Unsigned,
    );
    let exec = held.executor("exec-a", PRODUCER);
    let test_box = &held.test_box;
    let submit = |name: &str| {
        let started = test_box.bay_ok(&held.repo, &["start", name, "--paths", "src/"]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(format!("src/{name}.rs")), "x\n").unwrap();
        let (code, queued) = result_json(
            test_box
                .bay(&view)
                .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
                .arg("merge"),
        );
        assert_eq!(code, 0, "{queued}");
        started["change"].as_str().unwrap().to_owned()
    };
    // A first pass on its own, so the measured one starts from a runner that has run.
    submit("first");
    test_box.bay_ok(&exec, &["runner", "run", "--once"]);

    // GIVEN three changes waiting WHEN one pass lands them all
    let ids: Vec<String> = ["one", "two", "three"].into_iter().map(submit).collect();
    let trace = test_box.path().join("trace.jsonl");
    let (code, ran) = result_json(
        test_box
            .bay(&exec)
            .env("BAY_TEST_GIT_TRACE", &trace)
            .args(["runner", "run", "--once"]),
    );
    assert_eq!(code, 0, "{ran}");
    let on_main = test_box.trunk_change_ids(&held.remote);
    for id in &ids {
        assert!(on_main.contains(id), "{id} did not land: {ran}");
    }

    // THEN it makes at most 22 fetches (G680), 17 pushes and 2 listings (G681).
    for (sub, most) in [("fetch", 22), ("push", 17), ("ls-remote", 2)] {
        let made = calls(&trace, sub);
        assert!(
            made.len() <= most,
            "{} {sub}:\n{}",
            made.len(),
            made.join("\n")
        );
    }
}

//! G860 (a fleet measurement, 2026-11-22: one landing made 15 fetches and 7 pushes, and a batch
//! of three 21 fetches and 11 pushes, about 1.5 s each on a hosted remote). Three of them asked
//! the remote what it had just said:
//!
//! - a batch's members each pushed their landed record and terminal record after the publishing
//!   push, one push a member: now one atomic push carries every member's record and the log;
//! - each pass pushed this executor's log, when the remote already held its tip from the
//!   landing's own push: now a log whose tip this process pushed is not pushed again;
//! - the post work's stages each fetched main, the registry and the notes, milliseconds apart:
//!   now a plan reads each once for all its stages.
//!
//! So one landing made at most 14 fetches and 6 pushes, and a batch of three at most 20 fetches
//! and 8 pushes. G864: a landing's records ride its publishing push, whose chain, on the head
//! this process last pushed, stands for the read of the chain before it, so one landing makes at
//! most 13 fetches and 5 pushes. G865: a batch's records ride its publishing push too, so a batch
//! of three makes at most 17 fetches and 7 pushes; a batch whose records go apart (as a refused
//! push leaves them) still makes at most 20 and 8. G867: a batch's plan reads the chains and
//! leases of the line as the remote holds them now, so a place another runner took since the
//! pass fetched reads held: a fetch more a batch, at most 18 and 21. Read from the trace a test
//! build writes when `BAY_TEST_GIT_TRACE` names a file (G171).
//!
//! The heartbeat is longer than a pass, so no beat falls inside one: a beat's presence push comes
//! once a heartbeat, not once a landing (at `heartbeat_s = 1`, one landing made 10 pushes, 5 of
//! them beats), and a pass slowed by a loaded box made more of them.
//!
//! Guards to mutate: in `src/publish.rs` `land_batch`, leave each member's push mode `Now` (two
//! pushes more a batch whose records go apart); in `src/log.rs` `push`, drop the `on_remote` test
//! (a push more a pass); in `src/bisect.rs` `Fresh::tip`, fetch main each time (a fetch more a
//! pass); in `src/publish.rs` `land_with_records`, return a refusal at once (two fetches and a
//! push more a landing); in `batch_with_records`, return `Unsent` at once (three fetches and a push
//! more a batch of three).

#![cfg(unix)]

use std::path::Path;

use crate::support;

use support::PRODUCER;
use support::queued::{Check, host, ran_for};
use support::result_json;

const CONFIG: &str = "[runner]\nlease_s = 450\nheartbeat_s = 120\nparallel_checks = 4\nahead = 16\npost = true\nbisect_post = true\nbatch = { min = 2, max = 24, max_failure_rate = 0.3 }\n";

/// Two checks before the merge and one after it, as a fleet's policy has them.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"guards\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"src/**\"]\n\n[[check]]\nid = \"records\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"src/**\"]\n\n[[check]]\nid = \"slow\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\n";

/// The calls of Git subcommand `sub` one pass made, read from its trace.
fn calls(trace: &Path, sub: &str) -> Vec<String> {
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

/// Asserts the pass traced in `trace` made at most `fetches` fetches, `pushes` pushes and one
/// listing.
fn within(trace: &Path, fetches: usize, pushes: usize) {
    for (sub, most) in [("fetch", fetches), ("push", pushes), ("ls-remote", 1)] {
        let made = calls(trace, sub);
        assert!(
            made.len() <= most,
            "{} {sub}:\n{}",
            made.len(),
            made.join("\n")
        );
    }
}

#[test]
fn a_landing_and_a_batch_of_three_make_their_round_trips_once() {
    let held = host(&Check {
        config: CONFIG,
        policy: Some(POLICY),
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let exec = held.executor("exec", PRODUCER);
    // A first pass on its own, so the measured ones start from a runner that has run.
    let first = held.queue("first", "src");
    assert_eq!(ran_for(&held.pass(&exec), &first)["decision"], "merged");
    let traced = |name: &str, apart: bool| {
        let trace = held.test_box.path().join(name);
        let mut pass = held.test_box.bay(&exec);
        pass.env("BAY_TEST_GIT_TRACE", &trace);
        if apart {
            pass.env("BAY_TEST_LAND_APART", "1");
        }
        let (code, ran) = result_json(pass.args(["runner", "run", "--once"]));
        assert_eq!(code, 0, "{ran}");
        (trace, ran)
    };

    // GIVEN one change waiting WHEN a pass lands it THEN 13 fetches and 5 pushes at most
    let one = held.queue("one", "src");
    let (trace, ran) = traced("one.jsonl", false);
    assert_eq!(ran_for(&ran, &one)["decision"], "merged", "{ran}");
    within(&trace, 13, 5);

    // GIVEN three changes waiting WHEN a pass lands them in one batch THEN 18 fetches and 7
    // pushes at most
    let ids: Vec<String> = ["b1", "b2", "b3"]
        .into_iter()
        .map(|name| held.queue(name, "src"))
        .collect();
    let (trace, ran) = traced("three.jsonl", false);
    for id in &ids {
        assert_eq!(ran_for(&ran, id)["decision"], "merged", "{ran}");
    }
    within(&trace, 18, 7);

    // GIVEN three more WHEN a pass lands them in one batch whose records go apart from its
    // publishing push THEN 21 fetches and 8 pushes at most
    let ids: Vec<String> = ["c1", "c2", "c3"]
        .into_iter()
        .map(|name| held.queue(name, "src"))
        .collect();
    let (trace, ran) = traced("apart.jsonl", true);
    for id in &ids {
        assert_eq!(ran_for(&ran, id)["decision"], "merged", "{ran}");
    }
    within(&trace, 21, 8);
}

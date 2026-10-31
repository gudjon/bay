//! G600 (a fleet finding, 2026-10-31: one operation failed a check, then four runners claimed
//! it, built a new candidate on the same main and blocked it again, one after another, until
//! its author asked again). A pass lists its operations when it begins and visits each one
//! later. A runner that listed an operation while another ran it visited it after that runner
//! blocked it, and claimed it: a claim refused only an operation that ended.
//!
//! The guard to mutate, in `src/publish.rs`: the `Blocked` stop in `claim` (drop it, and a visit
//! on the chain read before the block claims the operation again). In `src/executor.rs`,
//! `unchanged` judged by the reason again fails its unit test.

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::PRODUCER;
use support::queued::{Check, Held, host};

/// The host's chain messages of change `id`, newest first.
fn chain(held: &Held, id: &str) -> String {
    held.test_box.git_bare(
        &held.remote,
        &["log", "--format=%s", &bay::change::change_ref(id)],
    )
}

/// The state of operation `op` in the host's record of change `id`.
fn state(held: &Held, id: &str, op: &str) -> Value {
    let record = held.test_box.git_bare(
        &held.remote,
        &[
            "show",
            &format!("{}:change.json", bay::change::change_ref(id)),
        ],
    );
    let record: Value = serde_json::from_str(&record).unwrap();
    record["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|operation| operation["op_id"] == op)
        .map(|operation| operation["state"].clone())
        .unwrap()
}

/// A pass's visit, in runner `clone`, of operation `op` of change `id`, on the chains `clone`
/// holds.
fn visit(clone: &Path, id: &str, op: &str) -> Value {
    let repo = bay::repo::Repo::open(clone).unwrap();
    let visited = bay::publish::execute(&repo, id, op, &bay::publish::Step::new("read")).unwrap();
    serde_json::from_str(&visited.to_json().unwrap()).unwrap()
}

/// Whether the host's queue names a place of `op`, as `clone` reads it.
fn placed(clone: &Path, op: &str) -> bool {
    let repo = bay::repo::Repo::open(clone).unwrap();
    bay::queue::Queue::read(&repo).unwrap().of(op).is_some()
}

#[test]
fn a_runner_never_claims_an_operation_another_runner_blocked() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "true",
    });
    let one = held.queue("one", "src");
    let op = format!("{one}.1");
    let first = held.executor("first", PRODUCER);
    let second = held.executor("second", PRODUCER);
    // GIVEN a runner that read the chain while the operation waited to run
    bay::repo::Repo::open(&second)
        .unwrap()
        .fetch_chains()
        .unwrap();
    // WHEN another runner checks it, and the check fails
    let pass = held.pass(&first);
    let blocked = chain(&held, &one);
    assert_eq!(state(&held, &one, &op), "blocked", "{blocked}\n{pass}");
    // THEN its visit on the chain it read before the block claims nothing and writes nothing
    let stale = visit(&second, &one, &op);
    assert_eq!(chain(&held, &one), blocked, "{stale}");
    assert_eq!(stale["reason"], "CHECK_FAILED", "{stale}");
    assert_eq!(stale["detail"]["unchanged"], true, "{stale}");
    // AND its visit on the chain read after the block takes no place in line either
    bay::repo::Repo::open(&second)
        .unwrap()
        .fetch_chains()
        .unwrap();
    let fresh = visit(&second, &one, &op);
    assert_eq!(chain(&held, &one), blocked, "{fresh}");
    assert_eq!(fresh["reason"], "CHECK_FAILED", "{fresh}");
    assert!(!placed(&second, &op), "{fresh}");
}

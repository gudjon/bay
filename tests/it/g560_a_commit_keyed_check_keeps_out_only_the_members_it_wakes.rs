//! G560. A pre-merge check keyed by its commit keeps out of a batch only the members whose diff
//! wakes it (BACKLOG 131).
//!
//! Such a check's record answers one commit, and a batch lands commits that check never saw,
//! so until 0.6.1 one such check kept every batch off, whatever its `when_paths` (a fleet
//! finding, 2026-10-29: a zero-edits check ran post for this reason alone). Now a member whose
//! diff wakes it is passed over, as a lane's member is, and lands alone in its own turn; the
//! members around it still batch.
//!
//! The guards to mutate: the `commit_keyed_woken` test in `throughput::plan_batch` (drop it
//! and the woken member lands in the batch; `break` for `continue` and the batch ends at it),
//! and the commit-keyed rule `no_batch` no longer holds (put it back and no batch forms).

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::PRODUCER;
use support::queued::{Check, Held, host};

const CONFIG: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\nbatch = { min = 2, max = 8 }\n";

/// `tests` wakes on every change; `zero`, keyed by its commit, on `docs/` only.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\n\n[[check]]\nid = \"zero\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nmaterialize = \"changed\"\nwhen_paths = [\"docs/**\"]\n";

fn held() -> Held {
    host(&Check {
        config: CONFIG,
        policy: Some(POLICY),
        seed: &[("src/a.rs", "fn a() {}\n"), ("docs/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    })
}

#[test]
fn members_that_wake_no_commit_keyed_check_batch() {
    let held = held();
    let one = held.queue("one", "src");
    let two = held.queue("two", "src");
    let executor = held.executor("exec", PRODUCER);
    let pass = held.pass(&executor);
    let landed = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        landed.contains(&one) && landed.contains(&two),
        "{landed:?}: {pass}"
    );
    assert_eq!(
        held.requests().len(),
        1,
        "one check for two landings, as one batch: {pass}"
    );
}

#[test]
fn a_member_that_wakes_one_is_passed_over_and_lands_alone() {
    let held = held();
    let one = held.queue("one", "src");
    let two = held.queue("two", "docs");
    let three = held.queue("three", "src");
    let executor = held.executor("exec", PRODUCER);
    let output = held
        .test_box
        .bay_process(&executor)
        .args(["--json", "runner", "run", "--once"])
        .output()
        .unwrap();
    let journal = String::from_utf8_lossy(&output.stderr).into_owned();
    let mut landed = held.test_box.trunk_change_ids(&held.remote);
    for _ in 0..3 {
        if landed.contains(&two) {
            break;
        }
        let _ = held.pass(&executor);
        landed = held.test_box.trunk_change_ids(&held.remote);
    }
    // Newest first: one and three landed as one batch, then two alone.
    assert_eq!(
        landed[..3],
        [two.clone(), three.clone(), one.clone()],
        "{journal}"
    );
    assert!(
        journal
            .lines()
            .any(|line| line.contains(&one) && line.contains("batch of 2 planned")),
        "{journal}"
    );
    // The batch's run of tests, then two's own runs of tests and zero.
    assert_eq!(held.requests().len(), 3, "{journal}");
}

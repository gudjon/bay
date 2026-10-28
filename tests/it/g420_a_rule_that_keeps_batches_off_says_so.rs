//! G420. A rule that keeps every batch off says so in the runner's journal (R7).
//!
//! A check keyed by its commit (`materialize = "changed"`, or a `commit` or `history` input)
//! never batches: its record never answers another commit. Measured 2026-10-27, one such check
//! kept a fleet's batches off for hours, and the journal said nothing, while a batch that ends
//! names why. Now the head's batch decision names the check.
//!
//! A post check judges main's tip after a landing, never a batch's candidate, so one keyed by
//! its commit keeps batches on.
//!
//! The guards to mutate, in `src/publish.rs`: the `eprintln!` of the `no batch` line in
//! `batch_plan` (drop it and the journal is silent again), and the `Stage::Post` test in
//! `no_batch` (drop it and a post check keeps every batch off).

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::PRODUCER;
use support::queued::{Check, host};

#[test]
fn a_check_keyed_by_its_commit_says_it_keeps_batches_off() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\nbatch = { min = 2, max = 8 }\n",
        policy: Some(
            "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nmaterialize = \"changed\"\n",
        ),
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let one = held.queue("one", "src");
    let _two = held.queue("two", "src");
    let executor = held.executor("exec", PRODUCER);
    let output = held
        .test_box
        .bay_process(&executor)
        .args(["--json", "runner", "run", "--once"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .find(|line| line.contains("no batch"))
        .unwrap_or_else(|| panic!("no line says why no batch formed: {stderr}"));
    assert!(
        line.contains(&one) && line.ends_with("no batch: check tests is keyed by its commit"),
        "{line}"
    );
}

#[test]
fn a_post_check_keyed_by_its_commit_keeps_batches_on() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\nbatch = { min = 2, max = 8 }\n",
        policy: Some(
            "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\n\n[[check]]\nid = \"later\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nmaterialize = \"changed\"\n",
        ),
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
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

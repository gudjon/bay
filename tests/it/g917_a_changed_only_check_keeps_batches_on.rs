//! G917. A check that sees only its changed paths keeps batches on.
//!
//! A changed-only check (`materialize = "changed"`, N-18) sees the diff of its candidate against
//! its base, so its record answers only a candidate with the same tree judged against a base
//! with the same tree. Until 0.7.34 it was keyed by its commit, and a head whose diff woke one
//! formed no batch (G560; a fleet ask, 2026-11-25: a records check that builds only the
//! changed files must not keep batches off). Now its record is keyed by the base's tree, and the batch tip, judged at
//! the batch's base by the host hook, has the tree and base tree of the batch's run.
//!
//! Here a whole-tree check reads what the head changes, so the change behind it does not pass it
//! (G703) and joins its batch: one run of each check for two landings.
//!
//! The guards to mutate: the `Key::Base` arm of `CheckEntry::key` (key it by commit and the
//! hook refuses the batch tip), and `changed_only` in `CheckEntry::commit_keyed` (put it back
//! and no batch forms).

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::PRODUCER;
use support::queued::{Check, host};

#[test]
fn two_changes_that_wake_a_changed_only_check_land_as_one_batch() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\nbatch = { min = 2, max = 8 }\n",
        policy: Some(
            "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\n\n[[check]]\nid = \"records\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nmaterialize = \"changed\"\n",
        ),
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let one = held.queue("one", "src");
    let two = held.queue("two", "src");
    let executor = held.executor("exec", PRODUCER);
    let output = held
        .test_box
        .bay_process(&executor)
        .args(["--json", "runner", "run", "--once"])
        .output()
        .unwrap();
    let journal = String::from_utf8_lossy(&output.stderr).into_owned();
    let landed = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        landed.contains(&one) && landed.contains(&two),
        "{landed:?}: {journal}"
    );
    assert!(
        journal
            .lines()
            .any(|line| line.contains(&one) && line.contains("batch of 2 planned")),
        "{journal}"
    );
    assert_eq!(
        held.requests().len(),
        2,
        "one run of each check for two landings, as one batch: {journal}"
    );
}

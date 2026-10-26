//! G256. A check made ahead of its turn lands its entry with no new run (NEW-2,
//! `crate::throughput`).
//!
//! With `[runner] ahead = 2` and exact-inputs reuse on a bay host, three changes wait in the
//! publication queue. While executor A checks the head, executor B (its own producer), which
//! may not run the head, checks the second and third entries on the bases the entries ahead of
//! them predict, and nothing else. Once A has landed the head, the second and third land with no
//! adapter run: each one's real merge is the predicted tree on the predicted base, and trunk's
//! history is the queue's order. When the head fails instead, the prediction missed: the second
//! entry's pre-check is about another tree, so it is checked again, as is the third. An entry
//! that changes the policy predicts nothing for the entries behind it: they are checked at their
//! turn, under the policy that then holds. Each request says which run it is: `turn` is
//! `"head"` for the head's run and `"ahead"` for a pre-check.
//!
//! The guards to mutate: `ahead_of_turn` in `src/publish.rs` (return `result` at once),
//! `throughput::predict` (return `trunk` without folding the entries ahead), and
//! `throughput::fold` (fold a policy change like any other). `predict`'s own policy test on a
//! persisted candidate backs `fold`'s: that candidate is the same merge, so either one stops.

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;

use support::PRODUCER;
use support::queued::{Check, Held, OTHER_PRODUCER, finished, host, ran_for};

const CONFIG: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 2\n";

/// Three queued changes; executor A checks the head, which holds, and executor B pre-checks
/// the second and third meanwhile. Returns the host, the three ids and A's pass.
fn while_the_head_is_checked(fail_head: bool) -> (Held, [String; 3], Value) {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: if fail_head { "[ \"$n\" = 1 ]" } else { "false" },
    });
    let ids = [
        held.queue("one", "src"),
        held.queue("two", "src"),
        held.queue("three", "src"),
    ];
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let child = held.spawn(&a);
    held.wait_held();
    // B may not run the head, which A holds: it checks the two entries behind it, and only them.
    let ahead = held.pass(&b);
    let requests = held.requests();
    assert_eq!(
        requests.len(),
        3,
        "the head's run and two pre-checks: {ahead}"
    );
    // The request says which run it is, so an adapter can give the head a worker first.
    assert_eq!(requests[0]["turn"], "head", "{:?}", requests[0]);
    for (at, id) in ids.iter().enumerate().skip(1) {
        assert_eq!(requests[at]["turn"], "ahead", "{:?}", requests[at]);
        let result = ran_for(&ahead, id);
        assert_eq!(result["decision"], "queued", "{result}");
        let precheck = &result["detail"]["precheck"];
        assert_eq!(precheck["on"], "predicted", "{result}");
        assert_eq!(
            precheck["tree"],
            requests[at]["candidate"]["tree"],
            "run {} is the pre-check of entry {}: {result}",
            at + 1,
            at + 1
        );
    }
    held.go();
    let outcome = finished(child);
    (held, ids, outcome)
}

#[test]
fn entries_checked_on_their_predicted_bases_land_with_no_new_run_in_queue_order() {
    let (held, [one, two, three], outcome) = while_the_head_is_checked(false);
    for id in [&one, &two, &three] {
        assert_eq!(ran_for(&outcome, id)["decision"], "merged", "{outcome}");
    }
    let requests = held.requests();
    assert_eq!(
        requests.len(),
        3,
        "the second and third landed on their pre-checks: {requests:?}"
    );
    assert_eq!(
        held.test_box.trunk_change_ids(&held.remote),
        vec![three, two, one],
        "trunk's history is the queue's order"
    );
    assert_eq!(
        held.tree("main"),
        requests[2]["candidate"]["tree"].as_str().unwrap(),
        "what landed last is the tree pre-checked for it"
    );
    assert_eq!(
        held.tree("main^"),
        requests[1]["candidate"]["tree"].as_str().unwrap(),
        "and the one before it"
    );
}

#[test]
fn a_failing_head_leaves_the_pre_checks_unused_and_the_entries_are_checked_again() {
    let (held, [one, two, three], outcome) = while_the_head_is_checked(true);
    assert_eq!(
        ran_for(&outcome, &one)["reason"],
        "CHECK_FAILED",
        "{outcome}"
    );
    for id in [&two, &three] {
        assert_eq!(ran_for(&outcome, id)["decision"], "merged", "{outcome}");
    }
    let requests = held.requests();
    assert_eq!(
        requests.len(),
        5,
        "each later entry checked again: {requests:?}"
    );
    assert_eq!(
        held.test_box.trunk_change_ids(&held.remote),
        vec![three, two]
    );
    let landed = held.tree("main^");
    assert_ne!(
        requests[1]["candidate"]["tree"].as_str().unwrap(),
        landed,
        "the second's pre-check was about a tree that never landed"
    );
    assert_eq!(
        requests[3]["candidate"]["tree"].as_str().unwrap(),
        landed,
        "its own check ran on the tree that landed"
    );
}

/// The seeded policy at its next revision: the same check, so only `revision` changes.
const NEXT_POLICY: &str = "revision = 2\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\n";

/// The entry at `changes` (0 the head, 1 the second) of three changes the policy; the head is
/// held in its check while B passes. Returns B's pass and the three ids.
fn behind_a_policy_change(changes: usize) -> (Value, [String; 3]) {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let queue = |at: usize, name: &str| {
        if at == changes {
            held.queue_with(name, &[".bay"], &[(".bay/policy.toml", NEXT_POLICY)])
        } else {
            held.queue(name, "src")
        }
    };
    let ids = [queue(0, "one"), queue(1, "two"), queue(2, "three")];
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let child = held.spawn(&a);
    held.wait_held();
    let ahead = held.pass(&b);
    held.go();
    let outcome = finished(child);
    assert_ne!(
        ran_for(&outcome, &ids[0])["decision"],
        "queued",
        "{outcome}"
    );
    (ahead, ids)
}

/// A queued entry that changes the policy is folded into no prediction.
#[test]
fn no_entry_behind_a_policy_change_is_checked_ahead() {
    let (ahead, [_, _, three]) = behind_a_policy_change(1);
    assert!(
        ran_for(&ahead, &three)["detail"]["precheck"].is_null(),
        "the second changes the policy, so the third has no predicted base: {ahead}"
    );
}

/// A head whose persisted candidate changes the policy predicts nothing either.
#[test]
fn no_entry_behind_a_head_that_changes_the_policy_is_checked_ahead() {
    let (ahead, [_, two, three]) = behind_a_policy_change(0);
    for id in [&two, &three] {
        assert!(
            ran_for(&ahead, id)["detail"]["precheck"].is_null(),
            "the head changes the policy, so nothing behind it has a predicted base: {ahead}"
        );
    }
}

/// A fleet finding (2026-10-26): a re-merge of a change that waits in line says where it
/// stands, never "adds it to main next" from 34th place, which its seat took for a refusal.
#[test]
fn a_queued_change_is_told_its_place_in_line() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let _one = held.queue("one", "src");
    let two = held.queue("two", "src");
    let _three = held.queue("three", "src");
    let (code, again) = support::result_json(held.test_box.bay(&held.repo).args(["merge", &two]));
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["detail"]["place"], 2, "{again}");
    assert_eq!(again["detail"]["in_line"], 3, "{again}");
    assert_eq!(
        again["say"], "Saved. It is place 2 of 3 in line; bay checks it when its turn comes.",
        "{again}"
    );
}

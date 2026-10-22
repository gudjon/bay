//! G318. A policy may reuse and batch unsigned evidence (owner ruling 2026-10-22).
//!
//! Where evidence counts unsigned (no key registry, or `[policy] signing = "approvals"`), the
//! team already takes an unsigned check record as that check's pass, so letting it stand for
//! an equal tree assumes no new trust. `[policy] reuse_unsigned = true` in the policy at the
//! base opens evidence reuse there exactly as a registry that signs everything opens it: a
//! pre-check runs ahead of its turn, a `footprint = true` record is carried onto a later
//! candidate whose read set is the same, byte for byte, and a batch is checked once. The
//! landing still names its trust (`unsigned`, `approvals`), and the carried record names the
//! record it carries. Without the key nothing changes: no pre-check runs, the check runs on
//! the entry's own candidate, and each change is checked alone.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/publish.rs` `Ctx::reuse_trusted`: drop its `|| matches!(policy::load_at(..), ..)`
//!   arm (no pre-check runs and nothing is carried or batched), or make the whole body `true`
//!   (the controls pre-check and batch).
//! - `src/publish.rs` `precheck`: drop `|| !ctx.reuse_trusted(trunk)` (the unsigned control
//!   pre-checks).
//! - `src/publish.rs` `reuses`: `ctx.reuse_trusted(judged.base)` to `ctx.signing().is_some()`
//!   (the pre-check runs and its record is never carried: the check runs again).
//! - `src/publish.rs` `batch_plan`: drop `!ctx.reuse_trusted(trunk) ||` (the unsigned batching
//!   control is checked once).
//! - `src/policy.rs` `read_at`: `reuse_unsigned: file.policy.reuse_unsigned` to
//!   `reuse_unsigned: false` (every keyed case runs as its control).

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;

use support::PRODUCER;
use support::queued::{Check, Held, OTHER_PRODUCER, Trust, finished, host_under, ran_for};

const CONFIG: &str = "[policy]\nevidence_reuse = \"exact-inputs\"\n\n[executor]\nlease_s = 30\nheartbeat_s = 1\nfootprint_lanes = true\n";

/// The policy: check `ui` on `ui/`, and check `api` on `api/`, both `footprint = true`, with
/// `table` as its `[policy]` table.
fn policy(table: &str) -> String {
    let check = |id: &str| {
        format!(
            "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"{id}/\"]\nfootprint = true\n"
        )
    };
    format!(
        "revision = 1\n{}{}\n[policy]\n{table}",
        check("ui"),
        check("api")
    )
}

/// What one footprint case saw.
struct Seen {
    held: Held,
    two: String,
    /// The check runs once executor B's pass ended, while A still held the head's run.
    runs_ahead: usize,
    /// Executor A's pass, which landed both.
    outcome: Value,
}

/// Under `trust` and the `[policy]` table `table`: the head (`ui/one.rs`) and the second
/// (`api/two.rs`) wait in the queue; A checks the head, which holds, while B passes once.
/// Then A lands both, in queue order.
fn footprint_case(trust: Trust, table: &str) -> Seen {
    let policy = policy(table);
    let held = host_under(
        &Check {
            config: CONFIG,
            policy: Some(&policy),
            seed: &[("ui/a.rs", "fn a() {}\n"), ("api/a.rs", "fn a() {}\n")],
            hold_first: true,
            fails: "false",
        },
        trust,
    );
    let one = held.queue("one", "ui");
    let two = held.queue("two", "api");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let child = held.spawn(&a);
    held.wait_held();
    let _ = held.pass(&b);
    let runs_ahead = held.requests().len();
    held.go();
    let outcome = finished(child);
    for id in [&one, &two] {
        assert_eq!(ran_for(&outcome, id)["decision"], "landed", "{outcome}");
    }
    assert_eq!(
        held.test_box.trunk_change_ids(&held.remote),
        vec![two.clone(), one],
        "the queue's order"
    );
    Seen {
        held,
        two,
        runs_ahead,
        outcome,
    }
}

/// The `api` record producer A holds on the landed tree.
fn landed_api_record(held: &Held) -> Value {
    let landed = held.tree("main");
    let records = held.records(PRODUCER, &landed);
    records
        .into_iter()
        .find(|record| record["check"]["id"] == "api")
        .unwrap_or_else(|| panic!("no api record on {landed}"))
}

/// With the key, B pre-checks the second on the tip, and A carries that record onto the
/// second's candidate: two runs in all, and the landing names its trust.
fn carried(seen: &Seen, trust: &str) {
    let requests = seen.held.requests();
    assert_eq!(
        seen.runs_ahead, 2,
        "the head's run and one pre-check: {requests:?}"
    );
    assert_eq!(requests.len(), 2, "carried, not rechecked: {requests:?}");
    let record = landed_api_record(&seen.held);
    assert_eq!(record["verdict"], "pass", "{record}");
    assert_eq!(
        record["footprint"]["from"]["tree"], requests[1]["candidate"]["tree"],
        "the carried record names the pre-check's: {record}"
    );
    let landed = ran_for(&seen.outcome, &seen.two);
    assert_eq!(landed["detail"]["trust"], trust, "{landed}");
}

#[test]
fn without_a_registry_the_key_carries_an_unsigned_record_and_says_unsigned() {
    let seen = footprint_case(Trust::Unsigned, "reuse_unsigned = true\n");
    carried(&seen, "unsigned");
}

#[test]
fn without_a_registry_and_without_the_key_the_check_runs_on_its_own_candidate() {
    let seen = footprint_case(Trust::Unsigned, "");
    let requests = seen.held.requests();
    assert_eq!(seen.runs_ahead, 1, "no pre-check: {requests:?}");
    assert_eq!(
        requests.len(),
        2,
        "the head's run and the second's: {requests:?}"
    );
    assert_eq!(
        requests[1]["candidate"]["tree"].as_str().unwrap(),
        seen.held.tree("main"),
        "the second's check ran on the tree that landed"
    );
    let record = landed_api_record(&seen.held);
    assert!(
        record["footprint"]["from"].is_null(),
        "not carried: {record}"
    );
    let landed = ran_for(&seen.outcome, &seen.two);
    assert_eq!(landed["detail"]["trust"], "unsigned", "{landed}");
}

#[test]
fn under_approvals_only_signing_the_key_carries_the_record_and_says_approvals() {
    let seen = footprint_case(
        Trust::Approvals,
        "signing = \"approvals\"\nreuse_unsigned = true\n",
    );
    carried(&seen, "approvals");
}

/// Three changes on `api/` wait in the queue of an unsigned host that batches up to eight;
/// one executor pass lands them all. Returns the check runs it took.
fn batch_runs(table: &str) -> usize {
    let config = "[policy]\nevidence_reuse = \"exact-inputs\"\n\n[executor]\nlease_s = 30\nheartbeat_s = 1\nbatch = { min = 2, max = 8 }\n";
    let policy = policy(table);
    let held = host_under(
        &Check {
            config,
            policy: Some(&policy),
            seed: &[("ui/a.rs", "fn a() {}\n"), ("api/a.rs", "fn a() {}\n")],
            hold_first: false,
            fails: "false",
        },
        Trust::Unsigned,
    );
    let ids = [
        held.queue("one", "api"),
        held.queue("two", "api"),
        held.queue("three", "api"),
    ];
    let executor = held.executor("exec", PRODUCER);
    let pass = held.pass(&executor);
    for id in &ids {
        assert_eq!(ran_for(&pass, id)["decision"], "landed", "{pass}");
        assert_eq!(ran_for(&pass, id)["detail"]["trust"], "unsigned", "{pass}");
    }
    held.requests().len()
}

#[test]
fn without_a_registry_the_key_batches_and_without_it_each_change_is_checked_alone() {
    assert_eq!(
        batch_runs("reuse_unsigned = true\n"),
        1,
        "one check for three"
    );
    assert_eq!(batch_runs(""), 3, "each checked alone");
}

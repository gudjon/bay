//! G310. A review-capacity cap limits what waits for a yes.
//!
//! The WIP limit belongs where the bottleneck is: a person's review. Under the opt-in policy
//! key `[limits] max_waiting_approvals = N`, while N live changes already wait for an approval
//! (`APPROVAL_PENDING`), `bay land` of a change whose paths need an approval check is refused
//! `WIP_LIMIT` (blocked, exit 10) before it is submitted, its detail `review` naming how many
//! wait, the cap and the one that has waited longest, and `detail.next` the step: answer or
//! finish those first. A change that needs no approval still lands. Without the key, both
//! changes wait for their approval as before.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/publish.rs` `review_full_at_submit`: `if queue.waiting < max {` (make it `if true {`:
//!   the second billing change waits `APPROVAL_PENDING` instead);
//! - `src/publish.rs` `review_full_at_submit`:
//!   `.any(|entry| entry.is_approval() && policy.requires(entry, &changed));` (drop
//!   `entry.is_approval() &&`: the `tests` check every change wakes makes the src-only change
//!   refused too);
//! - `src/flow.rs` `awaits_approval`: `.is_some_and(crate::publish::awaits_approval)` (make it
//!   `.is_some_and(|_| false)`: nothing counts as waiting, and the second billing change waits
//!   `APPROVAL_PENDING` instead).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::approval::{Trunk, trunk, trunk_with};

/// The cap: one change may wait for a yes.
const CAP: &str = "\n[limits]\nmax_waiting_approvals = 1\n";

/// A second change on `billing/`, which needs the approval check.
fn second_billing(trunk: &Trunk) -> (String, std::path::PathBuf) {
    trunk.start(
        "CARD-billing-2",
        &["billing/"],
        &[("billing/refunds.txt", "within 30 days\n")],
    )
}

#[test]
fn a_full_review_queue_refuses_a_change_that_needs_a_yes() {
    let trunk = trunk_with(CAP);
    let (held, _, _) = trunk.held_change();

    let (second, view) = second_billing(&trunk);
    let (code, refused) = trunk.land(&view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["decision"], "blocked", "{refused}");
    assert_eq!(refused["reason"], "WIP_LIMIT", "{refused}");
    assert_eq!(refused["change"], second.as_str(), "{refused}");
    let review = &refused["detail"]["review"];
    assert_eq!(review["waiting"], 1, "{refused}");
    assert_eq!(review["max"], 1, "{refused}");
    assert_eq!(review["oldest"], held.as_str(), "{refused}");
    assert!(review["age_s"].as_u64().is_some(), "{refused}");
    assert_eq!(
        refused["detail"]["next"], "answer or finish the changes waiting for a yes first",
        "{refused}"
    );
    assert!(refused["next_step"].is_string(), "{refused}");
    let say = refused["say"].as_str().unwrap();
    assert!(say.contains("wait for a person's OK"), "{say}");
}

#[test]
fn a_change_that_needs_no_approval_still_lands() {
    let trunk = trunk_with(CAP);
    trunk.held_change();

    let (_, view) = trunk.start(
        "CARD-src",
        &["src/"],
        &[("src/other.rs", "fn other() {}\n")],
    );
    let (code, landed) = trunk.land(&view);
    assert_eq!(code, 0, "{landed}");
}

#[test]
fn without_the_key_both_changes_wait_for_their_yes() {
    let trunk = trunk();
    trunk.held_change();

    let (_, view) = second_billing(&trunk);
    let (code, waiting) = trunk.land(&view);
    assert_eq!(code, 10, "{waiting}");
    assert_eq!(waiting["reason"], "APPROVAL_PENDING", "{waiting}");
}

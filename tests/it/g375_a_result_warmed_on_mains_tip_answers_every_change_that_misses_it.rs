//! G375 (items 1 and 2, owner ruling 2026-10-25: "get this fully built"). A result warmed on
//! main's tip answers every change that misses it.
//!
//! With `[runner] warm = true`, after its queue's work the runner runs each pre check with
//! `footprint = "observed"` once on main's newest tip, as a candidate of no change. A change on
//! that tip then reuses the record for every check whose observed reads it does not touch, and
//! lands with no run of it; a change that touches what the check read runs it. So a change
//! runs only the checks it reaches, and the work done for one tip is done once, for the fleet.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/bisect.rs` `plan`: drop the `warm` stage (the docs change runs the check itself).
//! - `src/publish.rs` `carry_over`: leave out the base tree (the warm record on main's tip is
//!   never looked at, and the docs change runs the check).

#![cfg(unix)]

use crate::support;

use support::PRODUCER;
use support::queued::{Check, host, ran_for};

const CONFIG: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\nwarm = true\n";

const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"lint\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nfootprint = \"observed\"\n";

#[test]
fn a_change_that_misses_what_the_check_read_lands_on_the_warmed_result() {
    let held = host(&Check {
        config: CONFIG,
        policy: Some(POLICY),
        seed: &[("src/a.rs", "fn a() {}\n"), ("docs/b.md", "b\n")],
        hold_first: false,
        // Every run reads src/a.rs, and nothing else of the tree.
        fails: "cat src/a.rs >/dev/null; false",
    });
    let runner = held.executor("exec-a", PRODUCER);

    // One pass warms the check on main's tip.
    let warmed = held.pass(&runner);
    assert_eq!(held.requests().len(), 1, "the tip was not warmed: {warmed}");

    // A change to docs/ only: the check's reads are untouched, so it lands with no run.
    let docs = held.queue("one", "docs");
    let landed = held.pass(&runner);
    assert_eq!(ran_for(&landed, &docs)["decision"], "merged", "{landed}");
    assert_eq!(
        held.requests().len(),
        1,
        "a change that misses the check's reads ran it: {landed}"
    );

    // A change to what the check read runs it.
    let src = held.queue_with("two", &["src"], &[("src/a.rs", "fn a() { 2 }\n")]);
    let landed = held.pass(&runner);
    assert_eq!(ran_for(&landed, &src)["decision"], "merged", "{landed}");
    assert_eq!(held.requests().len(), 2, "{landed}");
}

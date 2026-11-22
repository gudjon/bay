//! G859. A failure known before the run ends the run before it starts.
//!
//! A check whose last run failed ends the run, so a failing lint never pays for the tests behind
//! it (owner ruling 2026-10-25). A failure can also be known before the run starts: a record
//! reused or carried in place of a run answers the check `product_failure`, as a batch's
//! bisection record answers its culprit (G713). That failure ends the run before it starts: the
//! culprit is refused on the carried record, and no check after it runs alone on the culprit.
//! A fleet finding, 2026-11-22: a culprit refused on its carried lint held the head of the line
//! 19 minutes while the checks after the lint ran.
//!
//! Guard to mutate: `known_failures` in `src/publish.rs`: return no check from it, and the
//! culprit's tests run alone.

#![cfg(unix)]

use serde_json::Value;

use crate::support;

use support::PRODUCER;
use support::queued::{Check, host, ran_for};

const CONFIG: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\nfootprint_lanes = true\nbatch = { min = 2, max = 8, max_failure_rate = 1.0 }\n";

/// Two checks every change requires, in this order: `lint`, keyed by the files under `bad/`
/// it reads (`footprint = true`), which fails a tree whose `bad/x.rs` says so, and `tests`,
/// which reads the whole tree and always passes.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"lint\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\npaths = [\"bad/\"]\nfootprint = true\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\n";

/// Whether `request` asks for the check `id` on a candidate that changes `path`.
fn asks(request: &Value, id: &str, path: &str) -> bool {
    request["plan"]["id"] == id
        && request["changed_paths"]
            .as_array()
            .is_some_and(|paths| paths.iter().any(|changed| changed == path))
}

#[test]
fn a_culprit_refused_on_its_carried_failure_runs_no_check_after_it() {
    let held = host(&Check {
        config: CONFIG,
        policy: Some(POLICY),
        seed: &[("src/a.rs", "fn a() {}\n"), ("bad/x.rs", "ok\n")],
        hold_first: false,
        fails: "printf '%s' \"$req\" | grep -q '\"id\":\"lint\"' && grep -q bad bad/x.rs",
    });
    let one = held.queue("one", "src");
    let bad = held.queue_with("bad", &["bad"], &[("bad/x.rs", "bad\n")]);
    let three = held.queue("three", "src");
    let four = held.queue("four", "src");
    let executor = held.executor("exec", PRODUCER);
    let pass = held.pass(&executor);
    assert_eq!(ran_for(&pass, &bad)["reason"], "CHECK_FAILED", "{pass}");
    for id in [&one, &three, &four] {
        assert_eq!(ran_for(&pass, id)["decision"], "merged", "{pass}");
    }
    let requests = held.requests();
    // The bisection found the culprit on its lint, and the lint ended each of its runs.
    assert!(
        requests
            .iter()
            .any(|request| asks(request, "lint", "bad/x.rs")),
        "{requests:?}"
    );
    // No run of the tests ever read the culprit's change: not in the batch, not alone.
    assert!(
        !requests
            .iter()
            .any(|request| asks(request, "tests", "bad/x.rs")),
        "{requests:?}"
    );
}

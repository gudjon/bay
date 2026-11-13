//! G713. A batch's culprit is refused on the run that found it, never run alone again.
//!
//! A batch's bisection checks each prefix on the batch's base. Once the prefix before the
//! culprit lands, the culprit's own candidate is the failing prefix's tree, exactly, on a base
//! that prefix moved. A check whose verdict is keyed by what it reads (`footprint =
//! "observed"`) then reads the same bytes in that tree and in both bases, so the bisection's
//! failing record answers the culprit: it is refused at once on that record, carried onto its
//! candidate, with no run alone that repeats it. A check that read what the prefix changed is
//! run again, as before.
//!
//! Guard to mutate: the candidate's own tree in `carry_over`'s trees (`src/publish.rs`): drop
//! it, and the culprit is checked alone again.

#![cfg(unix)]

use crate::support;

use support::PRODUCER;
use support::queued::{Check, Held, host, ran_for};

const CONFIG: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\nfootprint_lanes = true\nbatch = { min = 2, max = 8, max_failure_rate = 1.0 }\n";

/// One check every change requires, keyed by the files it is given (`footprint = true`): `bad/`
/// and, with `reads_src`, `src/`, which the prefix changes. It fails a tree whose `bad/x.rs`
/// says so.
fn held(reads_src: bool) -> Held {
    let paths = if reads_src {
        "\"bad/\", \"src/\""
    } else {
        "\"bad/\""
    };
    let policy = format!(
        "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\npaths = [{paths}]\nfootprint = true\n"
    );
    host(&Check {
        config: CONFIG,
        policy: Some(&policy),
        seed: &[("src/a.rs", "fn a() {}\n"), ("bad/x.rs", "ok\n")],
        hold_first: false,
        fails: "grep -q bad bad/x.rs",
    })
}

/// Queues one, bad, three and four, runs the queue, and returns the check runs it took after
/// the bisection's three; `bad` is refused and the rest land.
fn runs_after_the_bisection(reads_src: bool) -> usize {
    let held = held(reads_src);
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
    // The whole batch, prefix 2 and prefix 1, each once.
    assert!(requests.len() >= 3, "{requests:?}");
    requests.len() - 3
}

#[test]
fn a_culprit_whose_reads_the_prefix_left_alone_is_refused_on_the_bisection() {
    // Only the last two, batched again: the culprit ran nothing of its own.
    assert_eq!(runs_after_the_bisection(false), 1);
}

#[test]
fn a_culprit_whose_reads_the_prefix_changed_is_checked_alone() {
    assert_eq!(runs_after_the_bisection(true), 2);
}

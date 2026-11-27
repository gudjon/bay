//! G938 (a load measurement, 2026-11-27: while main was red, its batches held one to three
//! members where five were planned, and the line grew from 4 to 113 changes). A batch on a red
//! main no longer ends before the first member on a red check's paths. The red post checks on
//! the paths of every member run on the batch's checks, judged against main as a lone
//! candidate's are ("merge unless worse", G677): the batch lands unless a member makes a red
//! worse, and then the bisection finds that member, which is refused as it would be alone.
//!
//! Guards to mutate, in `src/publish.rs` `batch`: the batch checked with the policy as loaded,
//! not `judging_red` (the second case lands the change that makes the red worse); `batch_reds`
//! left out, so only the head's paths count (the first case names no red check, the second
//! lands the worse change); the batch ended before the first member on a red's paths again (the
//! first case lands its head alone).

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::queued::{Check, Held, host, ran_for};
use support::{PRODUCER, passing_result};

/// `tests` runs on every change; `suite`, a post check, on `lib/` alone.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\n\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"lib/**\"]\n";

/// A host batching up to eight at any failure rate, whose main is red on `suite`: it fails
/// unit t1 while `lib/bad.rs` exists, and t1 and t2 while `lib/worse.rs` does too. Returns
/// the host and the runner's clone.
fn red_main() -> (Held, PathBuf) {
    let two = passing_result(2).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let fails = format!(
        "[ \"$1\" = suite ] && {{ [ -e lib/worse.rs ] && {{ {two}; exit 0; }}; [ -e lib/bad.rs ]; }}"
    );
    let held = host(&Check {
        config: "[runner]\npost = true\nbisect_post = true\nlease_s = 30\nheartbeat_s = 1\nbatch = { min = 2, max = 8, max_failure_rate = 1.0 }\n",
        policy: Some(POLICY),
        seed: &[("src/a.rs", "fn a() {}\n"), ("lib/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: &fails,
    });
    let executor = held.executor("exec", PRODUCER);
    // suite passes on a first landing on lib/, so the red that follows has a good end.
    let _good = held.queue("good", "lib");
    let _ = held.pass(&executor);
    let _ = held.pass(&executor);
    let _bad = held.queue("bad", "lib");
    for _ in 0..5 {
        let _ = held.pass(&executor);
        if red_on(&held, &executor) == Some("suite".to_owned()) {
            return (held, executor);
        }
    }
    let status = held.test_box.bay_ok(&executor, &["status"]);
    panic!(
        "main never went red on suite: {} {}",
        status["detail"]["red"], status["detail"]["red_pending"]
    );
}

/// The check main is red on first, if any, as the runner's clone `clone` judges it (the
/// agent's clone holds no post evidence until a status read fetches it).
fn red_on(held: &Held, clone: &Path) -> Option<String> {
    let status = held.test_box.bay_ok(clone, &["status"]);
    status["detail"]["red"][0]["check"]
        .as_str()
        .map(str::to_owned)
}

/// Each change id on the host's main with its `Bay-Batch` trailer (empty for none), oldest
/// first.
fn landings(held: &Held) -> Vec<(String, String)> {
    held.test_box
        .git_bare(
            &held.remote,
            &[
                "log",
                "--reverse",
                "--format=%(trailers:key=Change-Id,valueonly,separator=)|%(trailers:key=Bay-Batch,valueonly,separator=)",
                "main",
            ],
        )
        .lines()
        .filter_map(|line| line.split_once('|'))
        .filter(|(id, _)| !id.trim().is_empty())
        .map(|(id, batch)| (id.trim().to_owned(), batch.trim().to_owned()))
        .collect()
}

/// The batch id of each of `ids` as it landed, in their order; `None` for one not landed or
/// landed alone.
fn batches_of(landed: &[(String, String)], ids: &[&String]) -> Vec<Option<String>> {
    ids.iter()
        .map(|id| {
            landed
                .iter()
                .find(|(landed, _)| landed == *id)
                .and_then(|(_, batch)| batch.split_whitespace().next().map(str::to_owned))
        })
        .collect()
}

#[test]
fn a_member_on_a_red_checks_paths_lands_in_the_batch_when_it_adds_no_failure() {
    // GIVEN main red on suite, and three changes in line, the second on suite's paths
    let (held, executor) = red_main();
    let ids = [
        held.queue("one", "src"),
        held.queue("two", "lib"),
        held.queue("three", "src"),
    ];

    // WHEN a runner's pass checks them
    let pass = held.pass(&executor);

    // THEN the three land as one batch
    let landed = landings(&held);
    let batches = batches_of(&landed, &ids.iter().collect::<Vec<_>>());
    assert!(batches[0].is_some(), "the head landed alone: {landed:?}");
    assert!(
        batches.iter().all(|batch| *batch == batches[0]),
        "the batch ended before the member on the red's paths: {landed:?}"
    );
    // AND the batch ran suite, judged against main, which is still red on it
    assert_eq!(
        ran_for(&pass, &ids[0])["detail"]["red_checked"],
        serde_json::json!(["suite"]),
        "{pass}"
    );
    assert_eq!(red_on(&held, &executor).as_deref(), Some("suite"), "{pass}");
}

#[test]
fn a_member_that_makes_a_red_worse_is_found_and_refused_and_the_rest_land() {
    // GIVEN main red on suite, and four changes in line, the third adding a failure to suite
    let (held, executor) = red_main();
    let ids = [
        held.queue("one", "src"),
        held.queue("two", "src"),
        held.queue("worse", "lib"),
        held.queue("four", "src"),
    ];

    // WHEN a runner's pass checks them
    let pass = held.pass(&executor);

    // THEN the third is refused for the failure main does not have
    let refused = ran_for(&pass, &ids[2]);
    assert_eq!(refused["reason"], "CHECK_FAILED", "{pass}");
    let check = &refused["detail"]["checks"][0];
    assert_eq!(check["check"], "suite", "{pass}");
    assert_eq!(
        check["baseline_failures"],
        serde_json::json!(["t1"]),
        "{pass}"
    );
    // AND the others land in their order, the first two as one batch
    let landed = landings(&held);
    let order: Vec<&String> = landed
        .iter()
        .map(|(id, _)| id)
        .filter(|id| ids.contains(id))
        .collect();
    assert_eq!(order, vec![&ids[0], &ids[1], &ids[3]], "{landed:?}");
    let batches = batches_of(&landed, &[&ids[0], &ids[1]]);
    assert!(
        batches[0].is_some() && batches[0] == batches[1],
        "the first two did not land as one batch: {landed:?}"
    );
}

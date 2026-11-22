//! G861 (BACKLOG 248, fix A; a fleet measurement, 2026-11-20: 28 red batches were bisected one
//! check after another, and the line's head was held 10.3 hours in all). A change behind the
//! head that no planned check covers is checked alone on the trunk tip, ahead of its turn: its
//! screen. Here the check of a later batch, made ahead of its turn, fails, so it covers none of
//! its members, and each is screened. The one that fails alone is then passed over when the
//! batches are planned again: the others land in a batch whose check passed, with no bisection,
//! and it runs alone at its own turn, where it is refused.
//!
//! A runner plans a head's batch from the chains it read when its pass began, so a screen
//! written later in that pass is read by the next one. Here the head's runner ends its pass at
//! the first landing (`BAY_TEST_PASS_BOUND_S`), as a check longer than the bound ends it.
//!
//! Guards to mutate: in `src/throughput.rs` `plan_batch`, the `queued.screen == Some(false)`
//! test (the failing change joins the batch again, and the batch is checked and bisected); in
//! `src/publish.rs` `speculate_batch`, return `Ahead::Leave` for a member of a failed batch
//! (no screen is made).

#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host, ran_for};

const CONFIG: &str =
    "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 4\nbatch = { min = 2, max = 2 }\n";

/// The paths a check request names, sorted.
fn changed(request: &Value) -> Vec<String> {
    let mut paths: Vec<String> = request["changed_paths"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    paths.sort_unstable();
    paths
}

#[test]
fn a_change_that_fails_alone_is_screened_and_kept_out_of_the_next_batch() {
    // GIVEN a line of five changes, batched two at a time, and a check that fails every tree
    // holding src/bad.rs
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "[ -e src/bad.rs ]",
    });
    let one = held.queue("one", "src");
    let two = held.queue("two", "src");
    let three = held.queue("three", "src");
    let bad = held.queue("bad", "src");
    let five = held.queue("five", "src");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    // AND runner A holds the head's batch, one and two, and its check waits; its pass ends at
    // that batch's landing
    let runner_a = held
        .test_box
        .bay_process(&a)
        .env("BAY_TEST_PASS_BOUND_S", "0")
        .args(["--json", "runner", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    held.wait_held();

    // WHEN runner B makes a pass
    let first = held.pass(&b);

    // THEN it checks the next batch, three and bad, ahead of its turn, and that check fails
    assert_eq!(held.requests().len(), 3, "{first}");
    assert_eq!(
        changed(&held.requests()[1]),
        ["src/bad.rs", "src/three.rs"],
        "{first}"
    );
    // AND it screens bad alone on main, and bad fails its screen
    let screen = &ran_for(&first, &bad)["detail"]["precheck"];
    assert_eq!(screen["on"], "screen", "{first}");
    assert_eq!(screen["verdict"], "product_failure", "{first}");
    assert_eq!(changed(&held.requests()[2]), ["src/bad.rs"], "{first}");

    // WHEN runner B makes a second pass
    let second = held.pass(&b);

    // THEN the batch planned again passes over bad, and three and five are checked ahead
    assert_eq!(held.requests().len(), 4, "{second}");
    assert_eq!(
        changed(&held.requests()[3]),
        ["src/five.rs", "src/three.rs"],
        "{second}"
    );
    // AND bad is not screened again
    assert!(
        ran_for(&second, &bad)["detail"]["precheck"].is_null(),
        "{second}"
    );

    // WHEN the head's batch lands and runner A makes its next pass
    held.go();
    let landing = finished(runner_a);
    let after = held.pass(&a);

    // THEN three and five land on the record of their check ahead, with no run of their own
    let landed = held.test_box.trunk_change_ids(&held.remote);
    for id in [&one, &two, &three, &five] {
        assert!(landed.contains(id), "{id} did not land: {landing} {after}");
    }
    // AND bad runs alone at its turn and is refused
    assert!(!landed.contains(&bad), "{landed:?}");
    let refused = ran_for(&after, &bad);
    assert_eq!(refused["decision"], "blocked", "{after}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{after}");
    let requests = held.requests();
    assert_eq!(requests.len(), 5, "{after}");
    assert_eq!(changed(&requests[4]), ["src/bad.rs"], "{after}");
    // AND no batch holding bad was checked again, nor bisected
    let with_bad = requests
        .iter()
        .filter(|request| {
            let paths = changed(request);
            paths.len() > 1 && paths.iter().any(|path| path == "src/bad.rs")
        })
        .count();
    assert_eq!(with_bad, 1, "{after}");
}

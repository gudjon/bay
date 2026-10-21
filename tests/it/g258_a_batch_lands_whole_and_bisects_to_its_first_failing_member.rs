//! G258. A batch lands whole, and a failing one bisects to its first failing member
//! (`crate::throughput`, batching with bisection).
//!
//! With `[executor] batch` and exact-inputs reuse on a bay host, the queue's head and the
//! entries behind it are checked once as their combined tree on trunk. On a pass every member
//! lands in one push: each with its own executor-signed commit and `Change-Id`, parented on the
//! member before it, carrying `Bay-Batch`, so trunk's history is the queue's order and trunk
//! moved once. On a failure the bisection checks prefixes on the same tip, each once, and finds
//! the first failing member: the head lands alone on its prefix's records with no new run, the
//! culprit is checked alone as head and refused, and the members behind it batch again. While
//! the lane's recent failure rate is above `max_failure_rate`, no batch forms.
//!
//! The guards to mutate: the batch call in `run_attempts` (`src/publish.rs`; skip it and every
//! change is checked alone), `throughput::Bisect::record` (record every prefix as a pass: the
//! culprit lands), `throughput::roles` (accept a partial batch), and the rate test in
//! `batch_plan` (never refuse a batch).

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;

use support::PRODUCER;
use support::queued::{Check, Held, host, ran_for};

/// A trunk whose check fails a candidate that holds `src/bad.rs`, batching at most eight under
/// `rate`.
fn batching(rate: &str) -> Held {
    let config = format!(
        "[policy]\nevidence_reuse = \"exact-inputs\"\n\n[executor]\nlease_s = 30\nheartbeat_s = 1\nbatch = {{ min = 2, max = 8{rate} }}\n"
    );
    host(&Check {
        config: &config,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "[ -e src/bad.rs ]",
    })
}

/// The `Bay-Batch` trailers of trunk's newest `count` commits on the host, newest first.
fn batch_trailers(held: &Held, count: usize) -> Vec<String> {
    held.test_box
        .git_bare(
            &held.remote,
            &[
                "log",
                &format!("-{count}"),
                "--format=%(trailers:key=Bay-Batch,valueonly)",
                "main",
            ],
        )
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn landed(pass: &Value, id: &str) -> bool {
    ran_for(pass, id)["decision"] == "landed"
}

#[test]
fn a_passing_batch_lands_every_member_in_queue_order_on_one_check() {
    let held = batching("");
    let ids = [
        held.queue("one", "src"),
        held.queue("two", "src"),
        held.queue("three", "src"),
    ];
    let executor = held.executor("exec", PRODUCER);
    let pass = held.pass(&executor);
    for id in &ids {
        assert!(landed(&pass, id), "{pass}");
    }
    let requests = held.requests();
    assert_eq!(
        requests.len(),
        1,
        "one check for three landings: {requests:?}"
    );
    assert_eq!(
        requests[0]["candidate"]["tree"].as_str().unwrap(),
        held.tree("main"),
        "the checked tree is the tree that landed"
    );
    let [one, two, three] = ids;
    assert_eq!(
        held.test_box.trunk_change_ids(&held.remote),
        vec![three.clone(), two, one.clone()],
        "each member's own commit, in queue order"
    );
    let trailers = batch_trailers(&held, 3);
    assert_eq!(trailers.len(), 3, "{trailers:?}");
    let id = trailers[0].split_whitespace().next().unwrap().to_owned();
    assert_eq!(
        trailers,
        vec![
            format!("{id} 3/3"),
            format!("{id} 2/3"),
            format!("{id} 1/3")
        ]
    );
    let batch = &ran_for(&pass, &one)["detail"]["batch"];
    assert_eq!(batch["size"], 3, "{pass}");
    assert_eq!(batch["tree"], requests[0]["candidate"]["tree"], "{pass}");
    assert_eq!(batch["checks"].as_array().map(Vec::len), Some(1), "{pass}");
    assert_eq!(
        ran_for(&pass, &three)["detail"]["batch"]["position"],
        3,
        "{pass}"
    );
    // Every member's place was released in the publishing push: the queue is free.
    let queue = held.test_box.git_bare(
        &held.remote,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/bay/queue-done/",
        ],
    );
    assert_eq!(queue.lines().count(), 3, "{queue}");
}

/// Four changes, the second failing: the whole batch fails, the bisection checks prefixes 2
/// and 1, the head lands alone on its prefix's records, the culprit is checked alone and
/// refused, and with `rate` permitting it the last two batch again.
fn with_a_culprit(rate: &str) -> (Held, [String; 4], Value) {
    let held = batching(rate);
    let ids = [
        held.queue("one", "src"),
        held.queue("bad", "src"),
        held.queue("three", "src"),
        held.queue("four", "src"),
    ];
    let executor = held.executor("exec", PRODUCER);
    let pass = held.pass(&executor);
    let [one, bad, three, four] = &ids;
    for id in [one, three, four] {
        assert!(landed(&pass, id), "{pass}");
    }
    assert_eq!(ran_for(&pass, bad)["reason"], "CHECK_FAILED", "{pass}");
    assert_eq!(
        held.test_box.trunk_change_ids(&held.remote),
        vec![four.clone(), three.clone(), one.clone()]
    );
    let requests = held.requests();
    assert!(requests.len() >= 4, "{requests:?}");
    // The bisection: the whole batch, then the head and the culprit, then the head alone.
    assert_eq!(
        requests[2]["candidate"]["tree"].as_str().unwrap(),
        held.tree("main~2"),
        "the head's prefix is the tree it landed"
    );
    (held, ids, pass)
}

#[test]
fn a_failing_batch_bisects_to_its_culprit_and_the_rest_lands() {
    let (held, [_, _, three, four], pass) = with_a_culprit(", max_failure_rate = 1.0");
    let requests = held.requests();
    // Three bisection checks, the culprit alone, and the last two as one batch: no prefix
    // checked twice, and the head's landing ran nothing.
    assert_eq!(requests.len(), 5, "{requests:?}");
    assert_eq!(
        ran_for(&pass, &three)["detail"]["batch"]["size"],
        2,
        "{pass}"
    );
    assert_eq!(
        ran_for(&pass, &four)["detail"]["batch"]["position"],
        2,
        "{pass}"
    );
    assert_eq!(
        batch_trailers(&held, 3).len(),
        2,
        "only the second batch's members"
    );
}

#[test]
fn above_the_failure_rate_no_batch_forms() {
    let (held, [_, _, three, four], pass) = with_a_culprit("");
    // After the culprit's failure the lane's rate is 1 in 2, then 1 in 3, both above 0.3: the
    // last two are checked one by one.
    assert_eq!(held.requests().len(), 6, "{:?}", held.requests());
    for id in [&three, &four] {
        assert!(ran_for(&pass, id)["detail"]["batch"].is_null(), "{pass}");
    }
    assert_eq!(batch_trailers(&held, 3), Vec::<String>::new());
}

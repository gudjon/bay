//! G395 (a fleet finding, 2026-10-26: the head of the line was an operation that had ended,
//! superseded and its change abandoned, and its place was never released; each runner "took"
//! it after every one of its own operations, ran nothing, and said `ok`, for 25 minutes, while
//! 128 changes waited). A runner releases a head whose operation already ended between its own
//! operations, not only when its next pass starts.
//!
//! Guard to mutate, in `src/executor.rs`: the `finished_head` release in `abandoned_head`
//! (remove it).

#![cfg(unix)]

use crate::support;

use std::time::Duration;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};

const CONFIG: &str = "[runner]\nlease_s = 2\nheartbeat_s = 1\nahead = 2\n";

#[test]
fn a_head_that_ended_is_released_between_a_runners_operations() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        // Every run but the held first takes a while, so B is mid-pass when the head ends.
        fails: "sleep 6; false",
    });
    let one = held.queue("one", "src");
    let _two = held.queue("two", "src");
    let _three = held.queue("three", "src");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let mut runner_a = held.spawn(&a);
    held.wait_held();
    let runner_b = held.spawn(&b);
    std::thread::sleep(Duration::from_secs(3));
    runner_a.kill().unwrap();
    let _ = runner_a.wait();
    let (code, abandoned) = support::result_json(held.test_box.bay(&held.repo).args([
        "abandon",
        one.as_str(),
        "--why",
        "done elsewhere",
    ]));
    assert_eq!(code, 0, "{abandoned}");
    let pass = finished(runner_b);
    let released = held
        .test_box
        .git_bare(&held.remote, &["log", "--format=%s", "--all"])
        .lines()
        .any(|subject| subject.starts_with("release 1:"));
    assert!(released, "the ended head still holds its place: {pass}");
}

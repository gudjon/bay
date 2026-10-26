//! G399 (a fleet finding, 2026-10-26: five queue places outlived their operations, superseded,
//! abandoned, or reserved again after publishing, and each held the whole line for up to half
//! an hour once it reached the head). A runner releases every place whose operation has ended
//! at the start of each pass, wherever it waits in the line, not only the head's.
//!
//! Guard to mutate, in `src/executor.rs`: `sweep_ended` returning at once.

#![cfg(unix)]

use crate::support;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};

#[test]
fn an_abandoned_operations_place_is_released_while_the_head_is_held() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let _one = held.queue("one", "src");
    let two = held.queue("two", "src");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let runner_a = held.spawn(&a);
    held.wait_held();
    // two is abandoned while it waits behind the held head: its operation ends, its place stays.
    let (code, abandoned) = support::result_json(held.test_box.bay(&held.repo).args([
        "abandon",
        two.as_str(),
        "--why",
        "done elsewhere",
    ]));
    assert_eq!(code, 0, "{abandoned}");
    let pass = held.pass(&b);
    let released = held
        .test_box
        .git_bare(&held.remote, &["log", "--format=%s", "--all"])
        .lines()
        .any(|subject| subject.starts_with("release 2:"));
    assert!(released, "two.1's place still waits in the line: {pass}");
    held.go();
    let _ = finished(runner_a);
}

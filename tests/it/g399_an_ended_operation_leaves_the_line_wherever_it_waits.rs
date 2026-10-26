//! G399 (a fleet finding, 2026-10-26: five queue places outlived their operations, superseded,
//! abandoned, or reserved again after publishing, and each held the whole line for up to half
//! an hour once it reached the head). A runner releases every place whose operation has ended
//! at the start of each pass, wherever it waits in the line, not only the head's.
//!
//! Guard to mutate, in `src/executor.rs`: `sweep_ended` returning at once.

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};

#[test]
fn a_superseded_operations_place_is_released_while_the_head_is_held() {
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
    // A new revision of two supersedes two.1, whose place stays in the line behind the head.
    let started = held.test_box.bay_ok(&held.repo, &["start", "two"]);
    assert_eq!(started["change"], two.as_str(), "{started}");
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/two.rs"), "fn two() { 2; }\n").unwrap();
    held.test_box.bay_ok(&view, &["merge"]);
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

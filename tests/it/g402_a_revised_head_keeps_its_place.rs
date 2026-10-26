//! G402 (the trust-agents ruling, 2026-10-26; a fleet finding: a head revised to repair it was
//! released and joined at the back, behind 120 changes). A newer revision of the change at the
//! head takes the head's own place, as a newer revision of any queued change does; the host
//! takes the replacement.
//!
//! Guard to mutate, in `src/executor.rs`: `successor` returning `None` (the place of a
//! superseded head is released, and its new revision joins at the back).

#![cfg(unix)]

use crate::support;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};

#[test]
fn a_new_revision_of_the_head_runs_from_the_heads_place() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let one = held.queue("one", "src");
    let _two = held.queue("two", "src");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let runner_a = held.spawn(&a);
    held.wait_held();
    // A new revision of the head, made while A holds its first.
    let started = held.test_box.bay_ok(&held.repo, &["start", "one"]);
    assert_eq!(started["change"], one.as_str(), "{started}");
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/one.rs"), "fn one() { 1; }\n").unwrap();
    held.test_box.bay_ok(&view, &["merge"]);
    // B's pass meets the new revision while A still holds the head.
    let pass = held.pass(&b);
    let entry = |seq: u32| {
        held.test_box.git_bare(
            &held.remote,
            &["show", &format!("refs/bay/queue/{seq}:entry.json")],
        )
    };
    assert!(
        entry(1).contains(&format!("{one}.2")),
        "the head's place names the new revision: {} after {pass}",
        entry(1)
    );
    let places = held.test_box.git_bare(
        &held.remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/queue/"],
    );
    assert!(
        !places.contains("refs/bay/queue/3"),
        "no place at the back: {places}"
    );
    held.go();
    let _ = finished(runner_a);
}

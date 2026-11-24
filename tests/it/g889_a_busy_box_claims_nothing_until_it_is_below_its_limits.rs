//! BACKLOG 280 (an operator's ask, 2026-11-24: two runners' checks started beside another job
//! on a box at load1 30.5 and CPU some avg10 41 %, and that job's own load watch stopped it). A
//! box sets check-start load limits as Git config `bay.checkStartLoad1` and
//! `bay.checkStartCpuSome`. While it is at or above either, its runner's pass claims nothing and
//! writes nothing (`BOX_BUSY`); a limit it cannot read claims nothing either, and says why. At its
//! first pass below both, it takes the operation, which merges.
//!
//! Guard to mutate, in `src/publish.rs`: `box_busy` answering `None` (the busy box claims the
//! operation and runs its check).

#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::queued::{Check, Held, Trust, host_under};
use support::{PRODUCER, result_json};

/// One runner pass on `clone` at load1 `load1` and CPU some avg10 `some`.
fn pass_at(held: &Held, clone: &Path, load1: &str, some: &str) -> Value {
    let (code, result) = result_json(
        held.test_box
            .bay(clone)
            .env("BAY_TEST_LOAD1", load1)
            .env("BAY_TEST_CPU_SOME", some)
            .args(["runner", "run", "--once"]),
    );
    assert_eq!(code, 0, "{result}");
    result
}

/// The reason of the pass's row for change `change`.
fn reason_for(pass: &Value, change: &str) -> Option<String> {
    pass["detail"]["ran"]
        .as_array()?
        .iter()
        .find(|row| row["change"] == change)
        .and_then(|row| row["reason"].as_str().map(str::to_owned))
}

#[test]
fn a_busy_box_claims_nothing_until_it_is_below_its_limits() {
    let held = host_under(
        &Check {
            config: "",
            policy: None,
            seed: &[("src/a.rs", "fn a() {}\n")],
            hold_first: false,
            fails: "false",
        },
        Trust::Unsigned,
    );
    // GIVEN a runner whose box sets load limits of 24 and 20 %
    let runner = held.executor("exec-a", PRODUCER);
    let test_box = &held.test_box;
    test_box.git_ok(&runner, &["config", "bay.checkStartLoad1", "24"]);
    test_box.git_ok(&runner, &["config", "bay.checkStartCpuSome", "20"]);
    // AND a queued change
    let one = held.queue_with("one", &["src"], &[("src/b.rs", "fn b() {}\n")]);
    let chain = || {
        test_box.git_bare(
            &held.remote,
            &["log", "--format=%s", &format!("refs/bay/changes/{one}")],
        )
    };
    let claims = || {
        chain()
            .lines()
            .filter(|line| line.starts_with("claim "))
            .count()
    };

    // WHEN it passes at load1 30.5, then at CPU some 41 %
    let loaded = pass_at(&held, &runner, "30.5", "5");
    let pressed = pass_at(&held, &runner, "10", "41");
    // THEN each pass waits for the box and writes nothing on the change
    assert_eq!(
        reason_for(&loaded, &one).as_deref(),
        Some("BOX_BUSY"),
        "{loaded:#}"
    );
    assert_eq!(
        reason_for(&pressed, &one).as_deref(),
        Some("BOX_BUSY"),
        "{pressed:#}"
    );
    assert_eq!(claims(), 0, "a busy box claimed {one}:\n{}", chain());
    assert!(!test_box.trunk_change_ids(&held.remote).contains(&one));

    // WHEN a limit cannot be read
    test_box.git_ok(&runner, &["config", "bay.checkStartLoad1", "high"]);
    let unread = pass_at(&held, &runner, "1", "1");
    // THEN it claims nothing and says why
    assert_eq!(
        reason_for(&unread, &one).as_deref(),
        Some("BOX_BUSY"),
        "{unread:#}"
    );
    assert!(unread.to_string().contains("is not a number"), "{unread:#}");
    assert_eq!(
        claims(),
        0,
        "a box with an unread limit claimed {one}:\n{}",
        chain()
    );

    // WHEN the box is below both limits
    test_box.git_ok(&runner, &["config", "bay.checkStartLoad1", "24"]);
    let ran = vec![
        pass_at(&held, &runner, "10", "5"),
        pass_at(&held, &runner, "10", "5"),
    ];
    // THEN it takes the change, and the change merges
    assert!(
        test_box.trunk_change_ids(&held.remote).contains(&one),
        "{one} did not merge:\n{}\n{ran:#?}",
        chain()
    );
}

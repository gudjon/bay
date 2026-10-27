//! G421. A change that reached main around bay frees its queue place.
//!
//! Where landings are unsigned (no key registry, or `[policy] signing = "approvals"`), a
//! landing's trailers decide, so a push around bay can land a queued change's work with its
//! `Change-Id` and no `Revision-Id`. The head was then refused `CHANGE_ALREADY_MERGED` each
//! pass, the refusal was written nowhere, and its place held the whole line (measured
//! 2026-10-27: no landing for half an hour behind one such head). Now the refusal ends the
//! operation, and its place is released, so the change behind it lands.
//!
//! The guard to mutate: the block in `scanned` (`src/publish.rs`) that records the refusal;
//! drop it and the change behind never lands.

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::PRODUCER;
use support::queued::{Check, Trust, host_under};

#[test]
fn a_head_already_on_main_from_around_bay_lets_the_line_move() {
    let held = host_under(
        &Check {
            config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
            policy: None,
            seed: &[("src/a.rs", "fn a() {}\n")],
            hold_first: false,
            fails: "false",
        },
        Trust::Unsigned,
    );
    let one = held.queue("one", "src");
    let two = held.queue("two", "src");
    // Main moves around bay with the head's Change-Id and no Revision-Id.
    let bare = |args: &[&str]| held.test_box.git_bare(&held.remote, args).trim().to_owned();
    let old = bare(&["rev-parse", "main"]);
    let tree = bare(&["rev-parse", "main^{tree}"]);
    let message = format!("one, pushed around bay\n\nChange-Id: {one}\n");
    let outside = bare(&["commit-tree", &tree, "-p", &old, "-m", &message]);
    bare(&["update-ref", "refs/heads/main", &outside, &old]);
    let executor = held.executor("exec", PRODUCER);
    let mut landed = false;
    for _ in 0..3 {
        // A pass that refuses its head may exit non-zero; only what landed counts here.
        let _ = held
            .test_box
            .bay_process(&executor)
            .args(["--json", "runner", "run", "--once"])
            .output()
            .unwrap();
        if held.test_box.trunk_change_ids(&held.remote).contains(&two) {
            landed = true;
            break;
        }
    }
    assert!(
        landed,
        "two never landed behind a head already on main: {:?}",
        held.test_box.trunk_change_ids(&held.remote)
    );
}

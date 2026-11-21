//! G849 (a fleet finding, 2026-11-21: in a store no runner worked from, each status sweep took
//! about 80 s and brought about 148 MB, the same objects each time: the chains of 1315 ended
//! changes that a runner on another machine had cut by retention). A sweep fetched every chain the
//! remote moved but moved only the chains of live changes, so the cut of an ended change was
//! brought, never kept, and brought again. A sweep now moves every chain the store holds by the
//! rule of the chains it works on: forward, a retention cut taken, local work kept.
//!
//! Guard to mutate: in `src/status.rs` `settle_listed`, drop the moves of `settle_held`; the
//! reader's chain then stays at its landed head after the cut.

#![cfg(unix)]

use crate::support;

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use support::PRODUCER;
use support::queued::{Check, Held, host};

/// The remote's head of the chain of `change`.
fn remote_head(held: &Held, change: &str) -> String {
    held.test_box
        .git_bare(
            &held.remote,
            &["rev-parse", &format!("refs/bay/changes/{change}")],
        )
        .trim()
        .to_owned()
}

/// The record at the chain commit `commit`, read from the remote.
fn record_at(held: &Held, commit: &str) -> Value {
    let text = held
        .test_box
        .git_bare(&held.remote, &["show", &format!("{commit}:change.json")]);
    serde_json::from_str(&text).unwrap()
}

/// The head of the chain of `change` in the store `dir`.
fn local_head(held: &Held, dir: &Path, change: &str) -> String {
    held.test_box
        .git_ok(dir, &["rev-parse", &format!("refs/bay/changes/{change}")])
        .trim()
        .to_owned()
}

/// One status read in `dir`, once its sweep has ended.
fn status(held: &Held, dir: &Path) {
    let _ = held.test_box.bay_ok(dir, &["status"]);
    support::sweeps_ended(dir);
}

#[test]
fn a_sweep_keeps_the_retention_cut_of_an_ended_chain_this_store_holds() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n\n[retention]\nwindow_s = 5\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let runner = held.executor("exec-a", PRODUCER);
    let reader = held.test_box.clone_of(&held.remote, "reader");

    // GIVEN a store that read a change while it waited, then read it landed
    let change = held.queue("one", "src");
    status(&held, &reader);
    let _ = held.pass(&runner);
    let landed = remote_head(&held, &change);
    assert_eq!(record_at(&held, &landed)["state"], "landed");
    status(&held, &reader);
    assert_eq!(local_head(&held, &reader, &change), landed);

    // AND a runner on another machine that then cut its chain by retention
    let mut cut = landed.clone();
    let mut last = Value::Null;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_secs(1));
        last = held.pass(&runner);
        cut = remote_head(&held, &change);
        if record_at(&held, &cut)["retention_cut"] == true {
            break;
        }
    }
    assert_ne!(
        cut, landed,
        "the change was never cut; the last pass: {last}"
    );

    // WHEN the store reads its status again
    status(&held, &reader);

    // THEN its chain of the change is the cut, so no later sweep brings the cut again
    assert_eq!(local_head(&held, &reader, &change), cut);
}

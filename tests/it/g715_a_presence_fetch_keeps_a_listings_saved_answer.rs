//! G715 (a measurement, 2026-11-14: on a store whose agent sessions each fetch the fleet's
//! presence at a turn's start, no listing was ever served its saved answer). A presence ref that
//! moves, or a presence directory pruned away and made again, keeps a listing's saved answer (G714): whether a runner was seen lately is judged against
//! the clock, and a saved answer is never older than two minutes. Any other ref still ends it.
//!
//! Guard to mutate: in `src/saved.rs` `stamp`, the `held` stamp of `refs/bay/` (stamp it by its
//! time, and a presence directory made again ends the saved answer).

#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::result_json;

use crate::g617_a_listings_sweep_serves_the_reads_in_its_window::{OPEN, store};

const PRESENCE: [&str; 2] = [
    "refs/bay/fleet/reader-box",
    "refs/bay/fleet-executor/runner-box",
];

#[test]
fn a_presence_fetch_keeps_a_listings_saved_answer() {
    let (test_box, _, _, reader, id) = store();
    let read = || -> Value {
        let mut cmd = test_box.bay(&reader);
        cmd.arg("status")
            .env("BAY_ACTOR", "reader")
            .env("BAY_TEST_SWEPT_S", OPEN);
        let (code, result) = result_json(&mut cmd);
        assert_eq!(code, 0, "{result}");
        result
    };
    let chain = format!("refs/bay/changes/{id}");
    let tip = test_box.git_ok(&reader, &["rev-parse", &chain]);
    let tree = test_box.git_ok(&reader, &["rev-parse", &format!("{chain}^{{tree}}")]);

    // GIVEN a store that holds the fleet's presence, and a listing's saved answer
    for name in PRESENCE {
        test_box.git_ok(&reader, &["update-ref", name, tip.trim()]);
    }
    let _ = read();
    let _ = read();
    let saved = read();
    assert!(saved["detail"]["saved_s"].is_u64(), "{saved}");

    // WHEN a presence fetch moves the fleet's refs and brings a new one
    for name in PRESENCE {
        test_box.git_ok(&reader, &["update-ref", name, tree.trim()]);
    }
    test_box.git_ok(
        &reader,
        &["update-ref", "refs/bay/fleet/other-box", tip.trim()],
    );

    // THEN the next listing is still the saved answer
    let again = read();
    assert!(again["detail"]["saved_s"].is_u64(), "{again}");
}

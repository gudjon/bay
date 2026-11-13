//! G714 (a measurement, 2026-11-13: every `bay status` on a store of 6,292 change refs spent
//! 1.5-2.5 s rebuilding one answer while nothing in the store moved). A listing's answer is saved
//! with the times of the store's ref directories, packed refs and table list, and a listing that
//! finds them as they were answers it again with no Git at all (`detail.saved_s`). A ref any
//! writer moves, bay or plain Git, ends it: the next listing reads the store again.
//!
//! Guard to mutate: in `src/saved.rs` `answer`, the stamp comparison (a listing after a ref moved
//! is served the old answer).

#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::result_json;

use crate::g617_a_listings_sweep_serves_the_reads_in_its_window::{OPEN, store};

#[test]
fn a_listing_answers_again_while_the_store_stands_still() {
    let (test_box, _, _, reader, id) = store();
    let trace = test_box.path().join("trace.jsonl");
    let read = || -> (Value, usize) {
        let _ = std::fs::remove_file(&trace);
        let mut cmd = test_box.bay(&reader);
        cmd.arg("status")
            .env("BAY_ACTOR", "reader")
            .env("BAY_TEST_SWEPT_S", OPEN)
            .env("BAY_TEST_GIT_TRACE", &trace);
        let (code, result) = result_json(&mut cmd);
        assert_eq!(code, 0, "{result}");
        let calls = std::fs::read_to_string(&trace).map_or(0, |text| text.lines().count());
        (result, calls)
    };
    // GIVEN a listing that swept, and one that read the store whole in the sweep's window
    let (first, _) = read();
    assert!(first["detail"]["saved_s"].is_null(), "{first}");
    let (whole, _) = read();
    let ids = |result: &Value| result["detail"]["more_changes"].clone();

    // WHEN the store stands still, THEN the next listing is the saved answer, with no Git
    let (again, calls) = read();
    assert!(again["detail"]["saved_s"].is_u64(), "{again}");
    assert_eq!(
        calls,
        0,
        "{}",
        std::fs::read_to_string(&trace).unwrap_or_default()
    );
    assert_eq!(again["detail"]["changes"], whole["detail"]["changes"]);
    assert_eq!(ids(&again), ids(&whole));
    assert_eq!(again["main"], whole["main"], "{again}");

    // WHEN plain Git moves a ref the listing reads, THEN the next listing reads the store again
    let chain = format!("refs/bay/changes/{id}");
    let tip = test_box.git_ok(&reader, &["rev-parse", &chain]);
    test_box.git_ok(
        &reader,
        &["update-ref", "refs/bay/changes/zzzz2345", tip.trim()],
    );
    let (moved, calls) = read();
    assert!(moved["detail"]["saved_s"].is_null(), "{moved}");
    assert!(calls > 0, "{moved}");

    // and a listing after it is the new saved answer
    let (after, calls) = read();
    assert!(after["detail"]["saved_s"].is_u64(), "{after}");
    assert_eq!(calls, 0, "{after}");
}

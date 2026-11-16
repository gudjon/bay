//! G732 (a measurement, 2026-11-15: on a store read by 51 agent sessions, `bay status` took 4.4 s
//! at the median, most of it each read's own fetch, as reads came more than a sweep's window
//! apart). A read whose store a sweep left at most `[status] serve_stale_s` ago answers from the
//! store as it is at once, names its age in a `stale_read` warning, and starts one sweep that runs
//! on for the next read. With `serve_stale_s = 0` a read past the window waits for its own sweep.
//! `BAY_TEST_SERVE_STALE_S` sets the key in a test build.
//!
//! Guard to mutate: in `src/status.rs` `fetch_swept`, drop `serves_stale` from the choice of
//! `until`, and the read waits for its slow sweep and shows the new trunk.

use crate::g617_a_listings_sweep_serves_the_reads_in_its_window::{OPEN, move_trunk, store};
use crate::support;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, result_json};

/// `bay status` in `dir` under `env`: the result and how long it took.
fn status(test_box: &TestBox, dir: &Path, env: &[(&str, &str)]) -> (Value, Duration) {
    let mut cmd = test_box.bay(dir);
    cmd.arg("status")
        .env("BAY_ACTOR", "reader")
        .env("BAY_TEST_SAVED", "off");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let started = Instant::now();
    let (code, result) = result_json(&mut cmd);
    assert_eq!(code, 0, "{result}");
    (result, started.elapsed())
}

fn stale_read(result: &Value) -> Option<&Value> {
    result["detail"]["warnings"]
        .as_array()?
        .iter()
        .find(|warning| warning["code"] == "stale_read")
}

#[test]
fn a_read_answers_from_a_recent_sweep_and_the_sweep_it_starts_serves_the_next() {
    let (test_box, _, remote, reader, _) = store();
    // GIVEN a store a sweep has just read
    let (first, _) = status(&test_box, &reader, &[]);
    let main = first["detail"]["main"].clone();
    // The first read's own sweep ends before main moves, so it cannot bring the new main in
    // (a gate box at load 60 let it finish after the move).
    support::sweeps_ended(&reader);
    let next = move_trunk(&test_box, &remote);

    // WHEN a read past the window, inside serve_stale_s, finds the next sweep slow
    let (quick, took) = status(
        &test_box,
        &reader,
        &[
            ("BAY_TEST_SERVE_STALE_S", "600"),
            ("BAY_TEST_SWEEP_DELAY_MS", "5000"),
        ],
    );

    // THEN it answers the store as it is at once, and says how old
    assert!(
        took < Duration::from_secs(4),
        "the read took {took:?}: {quick}"
    );
    assert_eq!(quick["detail"]["main"], main, "{quick}");
    let warning = stale_read(&quick).unwrap_or_else(|| panic!("no stale_read warning: {quick}"));
    assert!(warning["stale_s"].is_u64(), "{warning}");

    // AND the sweep it started serves the next read with the trunk it brought
    support::sweeps_ended(&reader);
    let (after, _) = status(&test_box, &reader, &[("BAY_TEST_SWEPT_S", OPEN)]);
    assert_eq!(after["detail"]["main"], next.as_str(), "{after}");
    assert!(stale_read(&after).is_none(), "{after}");
}

#[test]
fn with_serve_stale_off_a_read_past_the_window_waits_for_its_own_sweep() {
    let (test_box, _, remote, reader, _) = store();
    let _ = status(&test_box, &reader, &[]);
    let next = move_trunk(&test_box, &remote);
    // The harness sets serve_stale_s to 0: the read sweeps and shows the trunk it brought.
    let (read, _) = status(&test_box, &reader, &[]);
    assert_eq!(read["detail"]["main"], next.as_str(), "{read}");
    assert!(stale_read(&read).is_none(), "{read}");
}

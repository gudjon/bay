//! G710 (a measurement, 2026-11-13: on a remote of 59,387 refs a sweep took 18 s, and every read
//! past a sweep's window waited its whole 3 s deadline, then answered from the store anyway). When
//! the store's last sweep took longer than a read may wait, the read waits for none: it answers
//! from the store at once, says how old that is (`stale_read`), and the sweep it starts runs on.
//!
//! Guard to mutate: in `src/status.rs` `fetch_swept`, the `last_took` test (the read waits for
//! the sweep again, up to its deadline).

#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};

use serde_json::Value;
use support::result_json;

use crate::g617_a_listings_sweep_serves_the_reads_in_its_window::store;

#[test]
fn a_read_never_waits_for_a_sweep_that_cannot_end_in_time() {
    let (test_box, _, _, reader, _) = store();
    let read = |env: &[(&str, &str)]| {
        let mut cmd = test_box.bay(&reader);
        cmd.arg("status").env("BAY_ACTOR", "reader");
        for (key, value) in env {
            cmd.env(key, value);
        }
        let started = Instant::now();
        let (code, result) = result_json(&mut cmd);
        assert_eq!(code, 0, "{result}");
        (result, started.elapsed())
    };
    let (first, _) = read(&[]);
    assert!(first["detail"]["fetched_at"].is_string(), "{first}");

    // GIVEN a store whose last sweep, long over, took longer than a read's deadline
    let path = reader.join(".git").join("bay").join("swept.json");
    let mut record: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let now = record["ended"].as_u64().unwrap();
    record["unix"] = (now - 6000).into();
    record["ended"] = (now - 4000).into();
    std::fs::write(&path, record.to_string()).unwrap();

    // WHEN a read finds the next sweep slow too
    let (late, took) = read(&[("BAY_TEST_SWEEP_DELAY_MS", "8000")]);

    // THEN it answers from the store at once and names how old that is
    let warning = late["detail"]["warnings"]
        .as_array()
        .and_then(|warnings| {
            warnings
                .iter()
                .find(|warning| warning["code"] == "stale_read")
        })
        .unwrap_or_else(|| panic!("no stale_read warning: {late}"));
    assert!(warning["waited_ms"].as_u64().unwrap() < 2000, "{warning}");
    assert!(
        took < Duration::from_secs(6),
        "the read took {took:?}: {late}"
    );
}

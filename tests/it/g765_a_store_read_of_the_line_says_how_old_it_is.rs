//! G765 (a fleet finding, 2026-11-16: a panel showed "0 in line" from `bay queue --deadline 0` on a
//! machine whose copy of the line was two hours old, 15 places against 23 live, and nothing said
//! how old; no sweep fetched the line). A store read of the line names its age (`age_s`: since
//! this store last fetched the line; null when it never did), and the status sweep brings the line
//! with the rest, so a place another machine took reaches this store's copy.
//!
//! Guards to mutate: in `src/line.rs` `read`, drop `age_s`; in `src/status.rs`, drop the line's
//! refspecs from the sweep, and the other machine's place never arrives.

#![cfg(unix)]

use crate::support;

use support::queued::{Check, host};

#[test]
fn a_store_read_of_the_line_says_how_old_it_is() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 60\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let reader = held.test_box.clone_of(&held.remote, "reader");

    // A store that never fetched the line says its age is unknown.
    let unknown = held.test_box.bay_ok(&reader, &["queue", "--deadline", "0"]);
    assert!(unknown["detail"]["age_s"].is_null(), "{unknown}");

    // WHEN another machine takes a place, and this one's status sweep runs
    let id = held.queue("elsewhere", "src");
    let _ = held.test_box.bay_ok(&reader, &["status"]);

    // THEN the store's copy of the line holds that place, and says it is fresh
    let line = held.test_box.bay_ok(&reader, &["queue", "--deadline", "0"]);
    let ids: Vec<&str> = line["detail"]["places"]
        .as_array()
        .unwrap()
        .iter()
        .map(|place| place["change"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![id.as_str()], "{line}");
    let age = line["detail"]["age_s"]
        .as_u64()
        .unwrap_or_else(|| panic!("no age: {line}"));
    assert!(age < 60, "{line}");
}

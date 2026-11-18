//! G755 (owner, 2026-11-16: busy people with a quick fix want to know at once when it is on
//! main, and a panel guessed the same "~15–40 min" for the 1st and the 30th in line). A change
//! waiting in line names its place, how many are ahead and how long the line is, read from this
//! store with no call to the remote; its estimate of when it is on main counts those places
//! (G786: `eta`, which replaced the pace of main this row once gave).
//!
//! Guard to mutate: in `src/status.rs` `describe`, drop `operation["line"]`, and no read of a
//! queued change says its place.

#![cfg(unix)]

use crate::support;

use support::queued::{Check, host};

#[test]
fn a_change_in_line_names_its_place_and_when_it_reaches_main() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 60\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let _one = held.queue("one", "src");
    let _two = held.queue("two", "lib");
    let three = held.queue("three", "doc");

    // WHEN its author reads the third change
    let read = held.test_box.bay_ok(&held.repo, &["status", &three]);

    // THEN it names its place, the two ahead of it, and its estimate counts them
    let line = &read["detail"]["changes"][0]["operation"]["line"];
    assert_eq!(line["place"], 3, "{read}");
    assert_eq!(line["ahead"], 2, "{read}");
    assert_eq!(line["of"], 3, "{read}");
    let eta = &read["detail"]["changes"][0]["eta"];
    assert_eq!(eta["stage"], "in_line", "{read}");
    assert_eq!(eta["ahead"], 2, "{read}");
}

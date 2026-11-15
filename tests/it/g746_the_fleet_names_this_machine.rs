//! G746 (a request of a status panel, 2026-11-15: it found its own machine among the fleet's rows
//! by turning its hostname into the name a row carries, which breaks on any other naming). `bay
//! status --all` names, as `detail.this_machine`, which of its machine rows is the machine it ran
//! on, by the name that row carries.
//!
//! Guard to mutate: in `src/fleet.rs`, drop `this_machine` from the answer, and the panel cannot
//! find its own row.

use crate::support;

use support::{TestBox, unchecked_files};

#[test]
fn the_fleet_read_names_the_row_of_the_machine_it_ran_on() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    // GIVEN this machine's summary on the remote, with its doctor verdict
    let _ = test_box.bay(&repo).args(["doctor"]).output().unwrap();
    test_box.bay_ok(&repo, &["status"]);

    // WHEN the fleet is read
    let fleet = test_box.bay_ok(&repo, &["status", "--all"]);

    // THEN it names this machine, and that name is one of its rows
    let here = fleet["detail"]["this_machine"]
        .as_str()
        .unwrap_or_else(|| panic!("no this_machine: {fleet}"));
    let rows = fleet["detail"]["machines"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        rows.iter().any(|row| row["machine"] == here),
        "no row is {here}: {fleet}"
    );
}

//! G748 (a fleet finding, 2026-11-16: about twenty agents at a time polled `bay status <change>`
//! in loops on a box at load 108, and each read built every section of a listing for its one
//! change, while the line it waited on starved for CPU). A read of one change builds its row,
//! its view's leftovers, main's red, its hold and the reader's talks; the fleet's sections (flow,
//! flakes, targets, quarantines, unattributed merges) read `null`, not read here, and a listing
//! still builds them all.
//!
//! Guard to mutate: in `src/status.rs` `read_sections`, set `one` false, and the read of one
//! change builds the flow again.

use crate::support;

use support::{TestBox, unchecked_files};

#[test]
fn a_read_of_one_change_builds_its_row_and_no_fleet_section() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();

    // WHEN one change is read, and the whole store is listed
    let one = test_box.bay_ok(&repo, &["status", &id]);
    let listing = test_box.bay_ok(&repo, &["status"]);

    // THEN the read of one change has its row and no fleet section; the listing has them
    assert_eq!(one["detail"]["changes"][0]["change"], id.as_str(), "{one}");
    for section in ["flow", "flakes", "targets", "quarantines", "unattributed"] {
        assert!(
            one["detail"][section].is_null(),
            "{section} was built: {one}"
        );
    }
    assert!(!listing["detail"]["flow"].is_null(), "{listing}");
    assert!(
        one["detail"]["talks"].is_array(),
        "the reader's talks were left out: {one}"
    );
}

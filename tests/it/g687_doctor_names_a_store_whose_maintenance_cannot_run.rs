//! G687 (a fleet finding, 2026-11-09: a runner's store was not repacked for 10 days. A run killed
//! on day one left `maintenance.lock` behind, and every later run found it taken; 2,340 packs and
//! 240,000 loose objects followed, and an empty ref file a killed write left stopped `git gc`
//! itself). `bay doctor` names a store whose maintenance cannot run: a `store-stuck` row, degraded,
//! with each finding (a maintenance lock held an hour or more, more than 300 packs, an empty ref
//! file) and the recovery. A store with none has no such row.
//!
//! Guards to mutate: in `src/doctor.rs` `stuck_store_row`, skip the lock's age (a lock left
//! behind is not named), or `empty_refs` (an empty ref file is not named).

#![cfg(unix)]

use crate::support;

use std::time::{Duration, SystemTime};

use support::{TestBox, check_files, passing_result, result_json};

fn stuck(doctor: &serde_json::Value) -> Option<&serde_json::Value> {
    doctor["detail"]["rows"]
        .as_array()?
        .iter()
        .find(|row| row["name"] == "store-stuck")
}

#[test]
fn doctor_names_a_store_whose_maintenance_cannot_run() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (_, healthy) = result_json(test_box.bay(&repo).arg("doctor"));
    assert!(stuck(&healthy).is_none(), "{healthy}");

    // GIVEN a maintenance lock a killed run left two hours ago, and an empty ref file
    let git = repo.join(".git");
    let lock = git.join("objects").join("maintenance.lock");
    let file = std::fs::File::create(&lock).unwrap();
    file.set_modified(SystemTime::now() - Duration::from_secs(7200))
        .unwrap();
    std::fs::create_dir_all(git.join("refs").join("bay").join("changes")).unwrap();
    std::fs::write(git.join("refs/bay/changes/emptyref"), "").unwrap();

    // WHEN doctor reads the store
    let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));

    // THEN it names both, and how to recover
    let row = stuck(&doctor).unwrap_or_else(|| panic!("no store-stuck row: {doctor}"));
    assert_eq!(row["state"], "degraded", "{row}");
    let findings = row["findings"].to_string();
    assert!(findings.contains("maintenance.lock"), "{row}");
    assert!(findings.contains("refs/bay/changes/emptyref"), "{row}");
    let recovery = row["recovery"].as_str().unwrap();
    assert!(recovery.contains("remove"), "{row}");
    assert!(recovery.contains("fetch it again"), "{row}");
}

//! G792 (owner, 2026-11-19: "lower disk space all over, and bay plays a role"; a measurement the
//! same day: one box's views held 6.4 GB and 2.7 GB of build output in two of 209 views, and no
//! command said so). Doctor's `store` row carries `disk`: the bytes of the object store, of bay's
//! own state beside it, and of each view, largest first, with the sum. A view over 1 GiB is named
//! in the row's notes; doctor frees nothing.
//!
//! Guards to mutate: in `src/disk.rs` `Footprint::bytes`, count nothing (every figure is 0); or
//! drop the sort in `footprint` (the larger view is not first).

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json, row};

#[test]
fn doctor_names_the_store_state_and_each_view_largest_first() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (small, _small_view) = test_box.start_and_edit(&repo);
    let (large, large_view) = test_box.start_and_edit(&repo);
    // Build output in one view: 3 MiB of bytes no change commits.
    std::fs::create_dir_all(large_view.join("target")).unwrap();
    std::fs::write(large_view.join("target/out.bin"), vec![7u8; 3 << 20]).unwrap();

    let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));
    let store = row(&doctor, "store").clone();
    let disk = &store["disk"];
    let views = disk["views"]
        .as_array()
        .unwrap_or_else(|| panic!("{store}"));
    assert_eq!(views.len(), 2, "{disk}");
    assert_eq!(views[0]["change"], large.as_str(), "largest first: {disk}");
    assert_eq!(views[1]["change"], small.as_str(), "{disk}");
    let large_bytes = views[0]["bytes"].as_u64().unwrap();
    assert!(large_bytes >= 3 << 20, "{disk}");
    assert!(views[1]["bytes"].as_u64().unwrap() < large_bytes, "{disk}");
    assert!(disk["store_bytes"].as_u64().unwrap() > 0, "{disk}");
    let sum = disk["store_bytes"].as_u64().unwrap()
        + disk["state_bytes"].as_u64().unwrap()
        + disk["views_bytes"].as_u64().unwrap();
    assert_eq!(disk["total_bytes"].as_u64().unwrap(), sum, "{disk}");
    // Nothing here is over 1 GiB, so no view is named.
    assert!(
        !store["notes"].to_string().contains("views over"),
        "{store}"
    );
}

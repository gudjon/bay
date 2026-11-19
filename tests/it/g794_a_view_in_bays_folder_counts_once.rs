//! G794 (a measurement, 2026-11-19: one clone's bay folder held 1.1 GB of views an older bay made
//! there; doctor's storage row counted them in `bay_bytes` and again in `views`). A view kept in
//! the repository's own bay folder counts as a view only.
//!
//! Guard to mutate: in `src/storage.rs` `row`, size bay's folder with `size_of` (no folders
//! skipped): `bay_bytes` then holds the view's bytes too.

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json, row};

#[test]
fn a_view_kept_in_bays_folder_counts_as_a_view_only() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    // A view where an older bay kept its views: inside the repository's bay folder.
    let views = repo.join(".git").join("bay").join("views");
    std::fs::create_dir_all(views.join("zzzz0000")).unwrap();
    std::fs::write(views.join("zzzz0000.journal"), "{}\n").unwrap();
    std::fs::write(views.join("zzzz0000").join("out.bin"), vec![7u8; 3 << 20]).unwrap();

    let (_, doctor) = result_json(
        test_box
            .bay(&repo)
            .arg("doctor")
            .env("BAY_TEST_STORAGE_BUDGET_MS", "60000"),
    );
    let storage = row(&doctor, "storage").clone();
    assert_eq!(storage["partial"], false, "{storage}");
    let bay = storage["bay_bytes"]
        .as_u64()
        .unwrap_or_else(|| panic!("{storage}"));
    let in_views = storage["views"]["bytes"].as_u64().unwrap_or_default();
    assert!(in_views >= 3 << 20, "the view counts as a view: {storage}");
    assert!(
        bay < 3 << 20,
        "the view counts once, not in bay_bytes too: {storage}"
    );
}

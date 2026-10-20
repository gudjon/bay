//! G172: doctor names the store and its maintenance (`DESIGN` § 15.2).
//!
//! bay creates no store and runs no maintenance; the site does both. Doctor's `store` row says
//! what the site chose: the promisor remote and its filter, the ref format, the views sharing
//! the store, whether the work tree is populated, `maintenance.strategy`, whether auto
//! maintenance is on, and whether the repository is registered for scheduled maintenance.
//! Every finding is advisory, never red.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json, row};

fn store(test_box: &TestBox, dir: &std::path::Path) -> serde_json::Value {
    let (_, doctor) = result_json(test_box.bay(dir).arg("doctor"));
    assert_ne!(doctor["decision"], "blocked", "{doctor}");
    let store = row(&doctor, "store").clone();
    assert_ne!(store["state"], "red", "{doctor}");
    store
}

fn notes(store: &serde_json::Value) -> String {
    store["notes"].to_string()
}

#[test]
fn doctor_names_each_kind_of_store() {
    let test_box = TestBox::new();
    let full = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &full,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );

    // A full clone with a work tree: no promisor, and the site's choice is named.
    let row = store(&test_box, &full);
    assert_eq!(row["promisor_remote"], serde_json::Value::Null, "{row}");
    assert_eq!(row["work_tree"], "populated", "{row}");
    assert_eq!(row["auto_maintenance"], true, "{row}");
    assert_eq!(row["scheduled"], false, "{row}");
    assert!(row["ref_format"].is_string(), "{row}");
    assert_eq!(row["state"], "advisory", "{row}");
    assert!(
        notes(&row).contains("git clone --filter=blob:none --no-checkout"),
        "{row}"
    );
    assert!(notes(&row).contains("git maintenance register"), "{row}");

    // A blob-less no-checkout cache: the promisor and filter are named, the work tree is empty,
    // and a view started in it is counted.
    let cache = test_box.path().join("cache");
    test_box.git_ok(
        test_box.path(),
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            "--no-checkout",
            &format!("file://{}", remote.display()),
            cache.to_str().unwrap(),
        ],
    );
    test_box.git_ok(&cache, &["config", "maintenance.strategy", "geometric"]);
    let row = store(&test_box, &cache);
    assert_eq!(row["promisor_remote"], "origin", "{row}");
    assert_eq!(row["filter"], "blob:none", "{row}");
    assert_eq!(row["work_tree"], "empty", "{row}");
    assert_eq!(row["maintenance_strategy"], "geometric", "{row}");
    assert_eq!(row["views"], 0, "{row}");
    test_box.bay_ok(&cache, &["start", "CARD-1", "--paths", "src/"]);
    assert_eq!(store(&test_box, &cache)["views"], 1);
    // Registered for scheduled maintenance, it says so.
    test_box.git_ok(
        &cache,
        &[
            "config",
            "--add",
            "maintenance.repo",
            cache.to_str().unwrap(),
        ],
    );
    let row = store(&test_box, &cache);
    assert_eq!(row["scheduled"], true, "{row}");
    assert_eq!(row["state"], "green", "{row}");

    // The same cache with its work tree checked out holds the blobs it checked out.
    test_box.git_ok(&cache, &["checkout", "-q", "main"]);
    let row = store(&test_box, &cache);
    assert_eq!(row["work_tree"], "populated", "{row}");
    assert!(notes(&row).contains("populated work tree"), "{row}");

    // A host with auto maintenance off.
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    test_box.git_bare(&host, &["config", "maintenance.auto", "false"]);
    let row = store(&test_box, &host);
    assert_eq!(row["work_tree"], "none", "{row}");
    assert_eq!(row["auto_maintenance"], false, "{row}");
    assert_eq!(row["state"], "advisory", "{row}");
    assert!(notes(&row).contains("auto maintenance is off"), "{row}");
}

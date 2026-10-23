//! G12. Failures record themselves.
//!
//! A failure that breaks the workspace and the push still leaves a receipt that later
//! reaches `status`. If the machine dies before the receipt is synchronized, `status` shows
//! the record as missing, never as success.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use support::{TestBox, check_files, passing_result, result_json};

fn set_writable(dir: &Path, writable: bool) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            set_writable(&path, writable);
        }
    }
    let mode = if writable { 0o755 } else { 0o555 };
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn a_failure_that_breaks_the_view_and_the_push_leaves_a_receipt() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);

    // Break the view and the remote's write side; reads still work.
    std::fs::remove_file(view.join(".git")).unwrap();
    set_writable(&remote.join("refs"), false);
    set_writable(&remote.join("objects"), false);

    let (code, result) = result_json(test_box.bay(&repo).args(["merge", "--wait", &id]));
    assert_eq!(code, 20, "{result}");
    let receipt_id = result["receipt"].as_str().unwrap().to_owned();
    assert!(receipt_id.starts_with("r-"), "{result}");

    // The receipt is a pending file outside the view, and status reports it with its age.
    let pending = repo
        .join(".git/bay/receipts/pending")
        .join(format!("{receipt_id}.json"));
    assert!(pending.is_file());
    assert!(!pending.starts_with(&view));
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let receipts = &status["detail"]["changes"][0]["receipts"];
    assert_eq!(receipts[0]["id"], receipt_id.as_str(), "{status}");
    assert_eq!(receipts[0]["synced"], false);
    assert_eq!(receipts[0]["step"], "freeze");
    assert!(receipts[0]["age_s"].is_u64());

    // When the remote takes writes again, the next bay command synchronizes the receipt.
    set_writable(&remote.join("refs"), true);
    set_writable(&remote.join("objects"), true);
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let receipts = &status["detail"]["changes"][0]["receipts"];
    assert_eq!(receipts[0]["id"], receipt_id.as_str(), "{status}");
    assert_eq!(receipts[0]["synced"], true);
    assert!(!pending.exists());
    let on_remote = test_box.git_bare(
        &remote,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    assert!(
        on_remote.contains(&receipt_id),
        "the remote chain holds the receipt"
    );
}

#[test]
fn a_machine_that_dies_before_the_sync_shows_a_missing_record_never_success() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);

    // The request reaches the remote, then the machine dies with whatever it held locally.
    let killed = test_box
        .bay(&view)
        .env("BAY_TEST_KILL_AT", "after-freeze")
        .args(["merge", "--wait"])
        .output()
        .unwrap();
    assert_eq!(killed.status.code(), Some(137));
    std::fs::remove_dir_all(&repo).unwrap();

    let other = test_box.clone_of(&remote, "other");
    let status = test_box.bay_ok(&other, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    assert_ne!(change["state"], "merged", "{status}");
    assert_eq!(change["operation"]["is_merged"], false);
    assert_eq!(change["operation"]["receipt"], "missing", "{status}");
}

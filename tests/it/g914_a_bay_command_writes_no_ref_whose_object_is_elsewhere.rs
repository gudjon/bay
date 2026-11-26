//! Item 325 (a fleet finding, 2026-11-25: one `bay status`, in a process whose environment named
//! another object directory with the store's own as its alternate, wrote 18 refs into a shared
//! store; their 150 objects went to that directory, which was then removed, and every fetch of
//! the store failed on them). A bay command that is not a hook judging a push runs Git outside any
//! object directory its environment names, so each ref it writes names an object in the store; and
//! `bay doctor` names a ref whose object the store does not hold, with the recovery.
//!
//! Guards to mutate: in `src/git.rs` `Git::new`, default every handle to `Quarantine::Inherited`
//! (the first case's refs name objects that are gone); in `src/doctor.rs` `stuck_store_row`, skip
//! `refs_without_objects` (the second case's ref is not named).

#![cfg(unix)]

use crate::support;

use std::path::Path;

use support::{TestBox, check_files, passing_result, result_json};

fn stuck(doctor: &serde_json::Value) -> Option<&serde_json::Value> {
    doctor["detail"]["rows"]
        .as_array()?
        .iter()
        .find(|row| row["name"] == "store-stuck")
}

/// Runs `git <args>` in `repo` as a process whose environment names `objects` for its object
/// directory and the store's own as its alternate.
fn git_in_foreign_objects(
    test_box: &TestBox,
    repo: &Path,
    objects: &Path,
    args: &[&str],
) -> String {
    let out = test_box
        .git(repo)
        .env("GIT_OBJECT_DIRECTORY", objects)
        .env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            repo.join(".git/objects"),
        )
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn bay_status_writes_no_ref_whose_object_stays_in_another_object_directory() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let other = test_box.clone_of(&remote, "other");
    // GIVEN a change another clone started, whose record the remote holds
    let (id, _) = test_box.start_and_edit(&repo);
    let record = format!("refs/bay/changes/{id}");
    assert!(
        !test_box
            .git_bare(&remote, &["for-each-ref", &record])
            .is_empty(),
        "the remote holds {record}"
    );

    // WHEN this clone's `bay status <id>` runs in a process whose environment names another
    // object directory, and that directory is then removed
    let foreign = test_box.path().join("foreign-objects");
    std::fs::create_dir_all(&foreign).unwrap();
    let (_, status) = result_json(
        test_box
            .bay(&other)
            .args(["status", &id])
            .env("GIT_OBJECT_DIRECTORY", &foreign)
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                other.join(".git/objects"),
            ),
    );
    support::sweeps_ended(&other);
    std::fs::remove_dir_all(&foreign).unwrap();

    // THEN it took the record, and every ref of the store names an object the store holds
    assert!(
        !test_box
            .git_ok(&other, &["for-each-ref", &record])
            .is_empty(),
        "bay status took {record}: {status}"
    );
    let refs = test_box.git(&other).arg("show-ref").output().unwrap();
    assert!(
        refs.status.success(),
        "{}",
        String::from_utf8_lossy(&refs.stderr)
    );
}

#[test]
fn doctor_names_a_ref_whose_object_the_store_does_not_hold() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (_, healthy) = result_json(test_box.bay(&repo).arg("doctor"));
    assert!(stuck(&healthy).is_none(), "{healthy}");

    // GIVEN a ref Git wrote while another object directory held its commit, since removed
    let foreign = test_box.path().join("foreign-objects");
    std::fs::create_dir_all(&foreign).unwrap();
    let tree = test_box.git_ok(&repo, &["rev-parse", "HEAD^{tree}"]);
    let lost = git_in_foreign_objects(
        &test_box,
        &repo,
        &foreign,
        &["commit-tree", "-m", "lost", &tree],
    );
    git_in_foreign_objects(
        &test_box,
        &repo,
        &foreign,
        &["update-ref", "refs/bay/changes/lost", &lost],
    );
    std::fs::remove_dir_all(&foreign).unwrap();

    // WHEN doctor reads the store
    let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));

    // THEN it names the ref and its object, and how to recover
    let row = stuck(&doctor).unwrap_or_else(|| panic!("no store-stuck row: {doctor}"));
    assert_eq!(row["state"], "degraded", "{row}");
    let named = row["findings"][0]["refs_without_objects"].to_string();
    assert!(
        named.contains(&format!("refs/bay/changes/lost {lost}")),
        "{row}"
    );
    let recovery = row["recovery"].as_str().unwrap();
    assert!(recovery.contains("git fetch"), "{row}");
    assert!(recovery.contains("git update-ref -d"), "{row}");
}

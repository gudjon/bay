//! G731 (a measurement, 2026-11-15: on a store of 6,300 changes a status spent 0.6 s verifying
//! evidence signatures, one `ssh-keygen` a record, the same records as the read before it). A
//! record's verdict depends only on its bytes, its signature and the keys its class file names,
//! so a yes is kept in the store's answers and every later read takes it from there: a second
//! status verifies no producer record again.
//!
//! Guard to mutate: in `src/evidence.rs` `verifies_in`, never read a kept verdict, and the second
//! status verifies the records again.

// The host hook and the ssh-keygen wrapper are shell scripts, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use support::{TestBox, check_files, passing_result, result_json, write_executable};

/// How many producer records one `bay status` in `repo` verified.
fn verified(test_box: &TestBox, repo: &Path, log: &Path) -> usize {
    let _ = std::fs::remove_file(log);
    let (code, status) = result_json(
        test_box
            .bay(repo)
            .args(["status"])
            .env("BAY_TEST_SAVED", "off"),
    );
    assert_eq!(code, 0, "{status}");
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains("bay-producer"))
        .count()
}

#[test]
fn a_second_status_verifies_no_producer_record_again() {
    let test_box = TestBox::new();
    let files = check_files("true", &passing_result(1), 1, 60);
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    // GIVEN a landing whose check evidence is producer-signed
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, out) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{out}");

    // Each `ssh-keygen` leaves its arguments on one line.
    let log = test_box.path().join("ssh-keygen-calls");
    write_executable(
        &test_box.fakebin.join("ssh-keygen"),
        &format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nexec /usr/bin/ssh-keygen \"$@\"\n",
            log.display()
        ),
    );

    // WHEN two statuses read the same store
    // No verdict kept yet: the merge kept its own, so its answers go.
    std::fs::remove_dir_all(repo.join(".git/bay/answers")).unwrap();
    let first = verified(&test_box, &repo, &log);
    let second = verified(&test_box, &repo, &log);

    // THEN the first verified the records and the second took every verdict from the answers
    assert!(first > 0, "the first status verified no producer record");
    assert_eq!(
        second, 0,
        "the second status verified {second} records again"
    );
}

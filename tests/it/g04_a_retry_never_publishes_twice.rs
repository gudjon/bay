//! G4. A retry never publishes twice.
//!
//! The push succeeds but the reply is lost: the retry finds the publication with the
//! change's `Change-Id` and `Revision-Id` on trunk and returns the first result, also when
//! the publication is executor-signed.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn the_reply_is_lost_after_the_push_and_the_retry_returns_the_first_result() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);

    let lost = test_box
        .bay(&view)
        .env("BAY_TEST_KILL_AT", "after-publish")
        .args(["merge", "--wait"])
        .output()
        .unwrap();
    assert_eq!(lost.status.code(), Some(137));
    let published = test_box.git_bare(&remote, &["rev-parse", "main"]);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);

    let (code, retry) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{retry}");
    assert_eq!(retry["decision"], "merged");
    assert_eq!(retry["detail"]["commit"], published.as_str());
    assert_eq!(retry["op_id"], format!("{id}.1"));
    // The publication found is this revision's: its `Revision-Id` is the retry's revision.
    assert_eq!(
        retry["detail"]["revision"],
        revision_id(&test_box, &remote, &published).as_str()
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        published
    );
}

/// The `Revision-Id` trailer of `commit` in `bare`.
fn revision_id(test_box: &TestBox, bare: &std::path::Path, commit: &str) -> String {
    test_box
        .git_bare(
            bare,
            &[
                "log",
                "-1",
                "--format=%(trailers:key=Revision-Id,valueonly)",
                commit,
            ],
        )
        .trim()
        .to_owned()
}

/// The match is on `Change-Id` and `Revision-Id`: after the lost reply the agent edits its
/// view, so its next land names a new revision of the same change. The publication on trunk
/// is of the earlier revision, so it is not this land's first result: the land is refused with
/// `CHANGE_ALREADY_MERGED` naming the revision that landed, and nothing is published again.
#[test]
fn a_publication_of_another_revision_is_not_the_first_result() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);

    let lost = test_box
        .bay(&view)
        .env("BAY_TEST_KILL_AT", "after-publish")
        .args(["merge", "--wait"])
        .output()
        .unwrap();
    assert_eq!(lost.status.code(), Some(137));
    let published = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let landed_revision = revision_id(&test_box, &remote, &published);
    assert_ne!(landed_revision, "");

    std::fs::write(view.join("src/new.rs"), "fn newer() {}\n").unwrap();
    let (code, retry) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{retry}");
    assert_ne!(retry["decision"], "merged", "{retry}");
    assert_eq!(retry["reason"], "CHANGE_ALREADY_MERGED", "{retry}");
    assert_eq!(retry["detail"]["merged"], published.as_str(), "{retry}");
    assert_eq!(
        retry["detail"]["merged_revision"],
        landed_revision.as_str(),
        "{retry}"
    );
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        published
    );
}

#[test]
fn the_retry_finds_the_executor_signed_publication() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let (id, view) = test_box.start_and_edit(&repo);

    let lost = test_box
        .bay(&view)
        .env("BAY_TEST_KILL_AT", "after-publish")
        .args(["merge", "--wait"])
        .output()
        .unwrap();
    assert_eq!(lost.status.code(), Some(137));
    let published = test_box.git_bare(&remote, &["rev-parse", "main"]);
    assert!(test_box.executor_signed(&remote, &keys, &published));

    let (code, retry) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{retry}");
    assert_eq!(retry["decision"], "merged");
    assert_eq!(retry["detail"]["commit"], published.as_str());
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

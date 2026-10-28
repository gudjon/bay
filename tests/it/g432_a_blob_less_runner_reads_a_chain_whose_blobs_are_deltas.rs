//! G432. A blob-less runner reads a change whose blobs come as deltas against ones it lacks.
//!
//! A runner's store is a blob-less clone. bay fetches its records with their contents
//! (`--no-filter`, since 0.5.24), and a change chain holds the change's own commit, so that
//! fetch brings the change's blobs too. The remote sends them as a thin pack: each changed
//! file a delta against its version on trunk, which a blob-less store never fetched. Git
//! fetches such a base from the promisor remote, and bay ran every call with lazy fetch off
//! (G104b), so the fetch failed whole: no chain of that pass arrived, and a change whose blobs
//! needed a base was never placed. Measured 2026-10-28, fourteen submitted changes no runner
//! held, every pass, on all three runners. Now a fetch that brings blobs may fetch the delta
//! bases its pack names; every other call still fetches nothing on its own.
//!
//! The guard to mutate: `fetches_blobs` in `Git::output` (`src/git.rs`); run the fetch with
//! lazy fetch off and the change is never read.

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json, unchecked_files};

/// A file large enough that an edit to one line travels as a delta against its old version.
fn big(edited: usize) -> String {
    (0..2000)
        .map(|n| {
            if n == edited {
                format!("line {n:04}: edited\n")
            } else {
                format!("line {n:04}: as it was seeded, long enough to be worth a delta\n")
            }
        })
        .collect()
}

/// Whether `store` holds `oid`, asked without fetching it.
fn holds(test_box: &TestBox, store: &Path, oid: &str) -> bool {
    test_box
        .git(store)
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["cat-file", "-e", oid])
        .output()
        .unwrap()
        .status
        .success()
}

#[test]
fn a_change_whose_blob_is_a_delta_against_one_the_runner_lacks_lands() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.git_bare(&remote, &["config", "uploadpack.allowFilter", "true"]);
    test_box.seed_with_check(
        &repo,
        &[("data/big.txt", &big(usize::MAX))],
        &unchecked_files(),
    );
    let executor = test_box.path().join("executor");
    test_box.git_ok(
        test_box.path(),
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            &format!("file://{}", remote.display()),
            executor.to_str().unwrap(),
        ],
    );
    // Trunk moves the file; the blob-less store fetches the commit but not its blob.
    std::fs::write(repo.join("data/big.txt"), big(1000)).unwrap();
    test_box.git_ok(&repo, &["commit", "-q", "-am", "trunk edits line 1000"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "HEAD:main"]);
    test_box.git_ok(&executor, &["fetch", "-q", "origin"]);
    let on_trunk = test_box.git_ok(&repo, &["rev-parse", "HEAD:data/big.txt"]);
    assert!(
        !holds(&test_box, &executor, on_trunk.trim()),
        "the store holds trunk's blob, so nothing comes as a delta against a missing one"
    );
    // A change edits another line of it and is submitted.
    let started = test_box.bay_ok(&repo, &["start", "edit-big", "--paths", "data"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("data/big.txt"), big(1500)).unwrap();
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let id = queued["change"].as_str().unwrap().to_owned();
    let (code, ran) = result_json(test_box.bay(&executor).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{ran}");
    assert!(
        test_box.trunk_change_ids(&remote).contains(&id),
        "the runner never read the change: {ran}"
    );
}

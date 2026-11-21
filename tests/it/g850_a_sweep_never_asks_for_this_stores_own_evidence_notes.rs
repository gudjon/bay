//! G850 (a fleet finding, 2026-11-21: in a store whose runner had stopped, each status sweep
//! took 63 to 72 s and added about 140 MB of packs, the same objects each time). The store's own
//! evidence notes were three commits ahead of the remote's: the runner wrote them in passes that
//! published nothing, and stopped. A sweep asked for every notes ref, its own too, though no read
//! takes this store's own; the remote's tip was then named by no ref of the store, git judged it
//! absent, and the whole history of the ref came again. A sweep now asks for every other writer's
//! notes only, by the refspecs of `Repo::fetch_evidence`.
//!
//! Guard to mutate: in `src/repo.rs` `evidence_refspecs`, drop the exclusions of this store's own
//! names; the sweep then brings the remote's copy of this store's notes.

#![cfg(unix)]

use crate::support;

use std::path::Path;

use support::{TestBox, result_json, unchecked_files};

/// This store's producer name, as `bay.producerName` holds it after its first write.
const PRODUCER: &str = "local-k3v9q2xa";

/// A commit of the empty tree with `message` in `dir`, or in the remote when `dir` is `None`.
fn empty_commit(test_box: &TestBox, remote: &Path, dir: Option<&Path>, message: &str) -> String {
    let run = |args: &[&str]| match dir {
        Some(dir) => test_box.git_ok(dir, args),
        None => test_box.git_bare(remote, args),
    };
    let tree = run(&["hash-object", "-w", "-t", "tree", "/dev/null"]);
    run(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t.invalid",
        "commit-tree",
        &tree,
        "-m",
        message,
    ])
}

/// Whether the store `dir` holds the object `oid`.
fn holds(test_box: &TestBox, dir: &Path, oid: &str) -> bool {
    test_box
        .git(dir)
        .args(["cat-file", "-e", oid])
        .status()
        .expect("git runs")
        .success()
}

#[test]
fn a_sweep_never_asks_the_remote_for_this_stores_own_evidence_notes() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let reader = test_box.clone_of(&remote, "reader");
    test_box.git_ok(
        &reader,
        &["config", "--local", "bay.producerName", PRODUCER],
    );

    // GIVEN notes this store wrote as its own producer, which the remote holds otherwise
    let own = format!("refs/notes/bay/evidence/{PRODUCER}");
    let mine = empty_commit(&test_box, &remote, Some(&reader), "notes of mine");
    test_box.git_ok(&reader, &["update-ref", &own, &mine]);
    let theirs = empty_commit(&test_box, &remote, None, "the remote's copy");
    test_box.git_bare(&remote, &["update-ref", &own, &theirs]);
    // AND notes another writer pushed, which this store lacks
    let other = "refs/notes/bay/evidence/ci";
    let ci = empty_commit(&test_box, &remote, None, "notes of ci");
    test_box.git_bare(&remote, &["update-ref", other, &ci]);

    // WHEN the store reads its status, and its sweep ends
    let (code, result) = result_json(
        test_box
            .bay(&reader)
            .arg("status")
            .env("BAY_ACTOR", "reader"),
    );
    assert_eq!(code, 0, "{result}");
    support::sweeps_ended(&reader);

    // THEN the sweep brought the other writer's notes
    assert_eq!(test_box.git_ok(&reader, &["rev-parse", other]), ci);
    // AND never asked for this store's own: their remote copy is not here, and its own stay
    assert!(
        !holds(&test_box, &reader, &theirs),
        "the sweep fetched the remote's copy of this store's own evidence notes"
    );
    assert_eq!(test_box.git_ok(&reader, &["rev-parse", &own]), mine);
}

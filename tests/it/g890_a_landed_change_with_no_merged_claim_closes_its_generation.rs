//! G890 (a fleet finding, 2026-11-24: 12 landed changes had their landed marker and no merged
//! claim; a start of each intent answered `CHANGE_ALREADY_MERGED`, and the same start with
//! `--new` answered `CHANGE_SUPERSEDED`, so the intent could open no further generation). A
//! change whose record reads landed and whose landed marker the remote holds closes its
//! generation, with or without its merged claim: a plain start and a `--new` start each open
//! the next generation, which supersedes it.
//!
//! Guard to mutate: in `src/start.rs` `open`, make `let landed = merged || (…);` read
//! `let landed = merged;`: the plain start answers `CHANGE_ALREADY_MERGED` and the `--new`
//! start `CHANGE_SUPERSEDED`.

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, unchecked_files};

/// A repository whose intent `CARD-890` has one landed change whose merged claim is gone from
/// the remote and from this store. The box, the repository and the landed change's id.
fn stranded() -> (TestBox, PathBuf, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let started = test_box.bay_ok(&repo, &["start", "CARD-890", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let claim = started["detail"]["intent"].as_str().unwrap().to_owned();
    assert!(claim.ends_with("/1"), "{started}");
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/b.rs"), "fn b() {}\n").unwrap();
    let (code, merged) = support::result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    let merged_claim = claim.replacen("refs/bay/intent/", "refs/bay/landed-intent/", 1);
    assert_ne!(merged_claim, claim, "{started}");
    assert!(
        !test_box
            .git_bare(&remote, &["for-each-ref", &merged_claim])
            .trim()
            .is_empty(),
        "the landing pushed no merged claim {merged_claim}"
    );
    // The merged claim is lost; the landed marker stays.
    test_box.git_bare(&remote, &["update-ref", "-d", &merged_claim]);
    // This store may never have fetched it.
    let _ = test_box
        .git(&repo)
        .args(["update-ref", "-d", &merged_claim])
        .output();
    let marker = format!("refs/bay/landed/{id}");
    assert!(
        !test_box
            .git_bare(&remote, &["for-each-ref", &marker])
            .trim()
            .is_empty(),
        "the landing pushed no marker {marker}"
    );
    (test_box, repo, id)
}

/// `bay start <args>` in `repo`: it opened generation 2 of `CARD-890`, superseding `landed`.
fn opens_generation_two(test_box: &TestBox, repo: &Path, args: &[&str], landed: &str) {
    let (code, started) = support::result_json(test_box.bay(repo).arg("start").args(args));
    assert_eq!(code, 0, "{started}");
    assert!(
        started["detail"]["intent"]
            .as_str()
            .is_some_and(|claim| claim.ends_with("/2")),
        "{started}"
    );
    assert_ne!(started["change"].as_str(), Some(landed), "{started}");
    assert_eq!(started["detail"]["supersedes"], landed, "{started}");
}

#[test]
fn a_plain_start_after_a_landing_with_no_merged_claim_opens_the_next_generation() {
    // GIVEN an intent whose landed change lost its merged claim
    let (test_box, repo, landed) = stranded();
    // WHEN the same intent starts again; THEN it opens generation 2, superseding the landing
    opens_generation_two(&test_box, &repo, &["CARD-890", "--paths", "src/"], &landed);
}

#[test]
fn a_new_start_after_a_landing_with_no_merged_claim_opens_the_next_generation() {
    // GIVEN an intent whose landed change lost its merged claim
    let (test_box, repo, landed) = stranded();
    // WHEN the same intent starts again with --new; THEN it opens generation 2
    opens_generation_two(
        &test_box,
        &repo,
        &["CARD-890", "--new", "--paths", "src/"],
        &landed,
    );
}

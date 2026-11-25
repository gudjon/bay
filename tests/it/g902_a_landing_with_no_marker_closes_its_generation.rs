//! G902 (a fleet finding, 2026-11-25: a host answered a batch's push with a fault and kept trunk,
//! the records and the first members' markers, but not the later members' landed and intent
//! markers; 20 landings in three days had no landed marker, and a start of one's intent adopted
//! the merged change while its `--new` answered `CHANGE_SUPERSEDED`). A change whose record reads
//! landed and whose one publication trunk holds closes its generation with no marker at all: a
//! plain start and a `--new` start each open the next generation. A marker is created only with
//! the update of main that merges it, so a lost one is never pushed again on its own: the trunk
//! scan reads the landing instead.
//!
//! Guard to mutate: in `src/start.rs` `open`, drop `|| published_on(…)`: the starts answer
//! `CHANGE_ALREADY_MERGED` and `CHANGE_SUPERSEDED`.

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json, unchecked_files};

/// A repository whose intent `CARD-902` has one landed change whose merged claim and landed
/// marker are gone from the remote and from this store. The box, the repository and the
/// landed change's id.
fn unmarked() -> (TestBox, PathBuf, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let started = test_box.bay_ok(&repo, &["start", "CARD-902", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let claim = started["detail"]["intent"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/b.rs"), "fn b() {}\n").unwrap();
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    let merged_claim = claim.replacen("refs/bay/intent/", "refs/bay/landed-intent/", 1);
    assert_ne!(merged_claim, claim, "{started}");
    let marker = format!("refs/bay/landed/{id}");
    for name in [&merged_claim, &marker] {
        assert!(
            !test_box
                .git_bare(&remote, &["for-each-ref", name])
                .trim()
                .is_empty(),
            "the landing pushed no {name}"
        );
        // The host lost it, and this store may never have fetched it.
        test_box.git_bare(&remote, &["update-ref", "-d", name]);
        let _ = test_box
            .git(&repo)
            .args(["update-ref", "-d", name])
            .output();
    }
    (test_box, repo, id)
}

/// `bay start <args>` in `repo`: it opened generation 2 of `CARD-902`, superseding `landed`.
fn opens_generation_two(test_box: &TestBox, repo: &Path, args: &[&str], landed: &str) {
    let (code, started) = result_json(test_box.bay(repo).arg("start").args(args));
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
fn a_plain_start_after_a_landing_with_no_marker_opens_the_next_generation() {
    // GIVEN an intent whose landed change lost its merged claim and its landed marker
    let (test_box, repo, landed) = unmarked();
    // WHEN the same intent starts again; THEN it opens generation 2, superseding the landing
    opens_generation_two(&test_box, &repo, &["CARD-902", "--paths", "src/"], &landed);
}

#[test]
fn a_new_start_after_a_landing_with_no_marker_opens_the_next_generation() {
    // GIVEN an intent whose landed change lost its merged claim and its landed marker
    let (test_box, repo, landed) = unmarked();
    // WHEN the same intent starts again with --new; THEN it opens generation 2
    opens_generation_two(
        &test_box,
        &repo,
        &["CARD-902", "--new", "--paths", "src/"],
        &landed,
    );
}

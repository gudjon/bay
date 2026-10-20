//! G161. A fix names the landing it fixes (`DESIGN` § 4, § 5; owner ruling Q2 in `plans/wave2.md`).
//!
//! `bay start <intent> --fixes <Change-Id>` records the landed change the new one fixes, and its
//! publication carries a `Fixes: <Change-Id>` trailer beside `Supersedes:` and `Reverts:`. The
//! named change must be on trunk, or the start is `CHANGE_NOT_LANDED` and nothing is created. A
//! fix is a change of its own intent: it claims generation 1 of that intent and never a
//! generation of the fixed change's intent, so it carries no `Supersedes:`. bay never infers the
//! link from subject words: a change whose intent merely says "fix" carries no `Fixes:`.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// A trunk with a passing check, and one landed change that adds `src/new.rs`.
fn landed_change() -> (TestBox, PathBuf, PathBuf, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    (test_box, repo, remote, id)
}

/// The remote trunk's tip trailer `key` (empty when the tip has none).
fn tip_trailer(test_box: &TestBox, remote: &Path, key: &str) -> String {
    test_box
        .git_bare(
            remote,
            &[
                "log",
                "-1",
                &format!("--format=%(trailers:key={key},valueonly)"),
                "main",
            ],
        )
        .trim()
        .to_owned()
}

fn refs(test_box: &TestBox, remote: &Path, prefix: &str) -> Vec<String> {
    test_box
        .git_bare(
            remote,
            &["for-each-ref", "--format=%(objectname) %(refname)", prefix],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_fix_lands_with_a_fixes_trailer_and_claims_no_generation_of_the_fixed_intent() {
    let (test_box, repo, remote, id) = landed_change();
    let claims = refs(&test_box, &remote, "refs/bay/intent/");

    let started = test_box.bay_ok(
        &repo,
        &["start", "CARD-fix", "--paths", "src/", "--fixes", &id],
    );
    let fix = started["change"].as_str().unwrap().to_owned();
    assert_eq!(started["detail"]["fixes"], id.as_str(), "{started}");
    // The fix claims generation 1 of its own intent, beside the fixed change's untouched claim.
    assert!(
        started["detail"]["intent"]
            .as_str()
            .unwrap()
            .ends_with("/1"),
        "{started}"
    );
    let claims_after = refs(&test_box, &remote, "refs/bay/intent/");
    assert!(claims.iter().all(|claim| claims_after.contains(claim)));
    assert_eq!(claims_after.len(), claims.len() + 1, "{claims_after:?}");

    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() { fixed() }\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote)[0], fix);
    assert_eq!(tip_trailer(&test_box, &remote, "Fixes"), id);
    assert_eq!(tip_trailer(&test_box, &remote, "Supersedes"), "");
    assert_eq!(tip_trailer(&test_box, &remote, "Reverts"), "");

    // The fixed change's intent is still at its first generation: `start --new` on it claims 2.
    let landed_intents = refs(&test_box, &remote, "refs/bay/landed-intent/");
    assert!(
        landed_intents.iter().all(|name| name.ends_with("/1")),
        "{landed_intents:?}"
    );
}

#[test]
fn a_fix_of_an_unlanded_change_is_change_not_landed_and_creates_nothing() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (open, _view) = test_box.start_and_edit(&repo);
    let changes = refs(&test_box, &remote, "refs/bay/changes/");
    let claims = refs(&test_box, &remote, "refs/bay/intent/");

    let (code, refused) = result_json(
        test_box
            .bay(&repo)
            .args(["start", "CARD-fix", "--paths", "src/", "--fixes", &open]),
    );
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_NOT_LANDED", "{refused}");
    assert_eq!(refused["change"], open.as_str(), "{refused}");
    assert_eq!(refs(&test_box, &remote, "refs/bay/changes/"), changes);
    assert_eq!(refs(&test_box, &remote, "refs/bay/intent/"), claims);

    // A name that is no change id at all is refused too, and creates nothing.
    let (code, bad) = result_json(test_box.bay(&repo).args([
        "start",
        "CARD-fix",
        "--paths",
        "src/",
        "--fixes",
        "not-a-change",
    ]));
    assert_ne!(code, 0, "{bad}");
    assert_eq!(refs(&test_box, &remote, "refs/bay/changes/"), changes);
}

#[test]
fn a_change_whose_intent_says_fix_carries_no_fixes_trailer() {
    let (test_box, repo, remote, id) = landed_change();
    let intent = format!("fix {id}");
    let started = test_box.bay_ok(&repo, &["start", &intent, "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() { 2 }\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(tip_trailer(&test_box, &remote, "Fixes"), "");
}

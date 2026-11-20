//! G820 (a fleet finding, 2026-11-20). bay removes a merged change's view once the change ends.
//! A `bay merge <change> --wait` asked again after that (the first wait was cut off by a session
//! restart, so asking again is the recovery) answered `VIEW_INCOMPLETE`, "an earlier step was
//! interrupted", on a change already merged. An ended change is answered before its view is read
//! (G688), but only one that ended with nothing to add or behind a newer one, so a merged one went
//! on to read its gone view. With no view, the record answers: merged, with its commit, and
//! nothing is written.
//!
//! Guards to mutate, in `src/publish.rs` `submit_land`: the `ChangeState::Landed` answer before
//! the freeze (the wait answers `VIEW_INCOMPLETE`); its test that the view is gone (a newer
//! revision in a kept view is answered merged, and G682 and G754 fail).

// Each case runs shell scripts (the check's adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_merged_change_whose_view_is_gone_answers_merged() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    let landing = test_box.git_bare(&remote, &["rev-parse", "main"]);

    // The next start releases the ended change's view, as bay does.
    test_box.bay_ok(&repo, &["start", "CARD-2", "--paths", "src/"]);
    assert!(!view.exists(), "the merged change's view stayed");

    let (code, again) = result_json(test_box.bay(&repo).args(["merge", id.as_str(), "--wait"]));

    assert_eq!(code, 0, "{again}");
    assert_eq!(again["decision"], "merged", "{again}");
    assert_eq!(again["detail"]["commit"], landing.as_str(), "{again}");
    let text = test_box.git_ok(
        &repo,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    let record: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(record["state"], "landed", "{record}");
    assert_eq!(
        record["operations"].as_array().unwrap().len(),
        1,
        "no operation is written: {record}"
    );
}

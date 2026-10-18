//! G10. Every refusal says what to do next.
//!
//! Every reason code in bay's table (`INTERFACES` § 2) returns `allowed` and `retry_when`; a
//! repeated unchanged retry is refused without work.

mod support;

use bay::result::{BayResult, Reason};
use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn every_reason_code_returns_allowed_and_retry_when() {
    // Every code in bay's table (`INTERFACES` § 2) names itself and carries both fields.
    assert!(Reason::ALL.len() >= 29, "{:?}", Reason::ALL);
    for &reason in Reason::ALL {
        let code = reason.code();
        assert!(
            code.chars().all(|c| c.is_ascii_uppercase() || c == '_') && code.contains('_'),
            "{code}"
        );
        assert_eq!(Reason::from_code(code), Some(reason), "{code}");
        let json: serde_json::Value =
            serde_json::from_str(&BayResult::blocked(reason).to_json().unwrap()).unwrap();
        assert_eq!(json["reason"], code);
        assert!(json["allowed"].is_array(), "{code}");
        assert!(
            json["retry_when"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "{code}"
        );
    }
}

#[test]
fn an_unchanged_retry_is_refused_without_work() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let runs = test_box.path().join("check-runs");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let before = format!("echo run >> '{}'", runs.display());
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(&before, &failing, 1, 60),
    );
    let (_, view) = test_box.start_and_edit(&repo);

    let (code, first) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{first}");
    assert_eq!(first["reason"], "CHECK_FAILED");
    assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().count(), 1);

    // Nothing changed: the same code, at once, and the check does not run again.
    let (code, again) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{again}");
    assert_eq!(again["reason"], "CHECK_FAILED");
    assert_eq!(again["detail"]["unchanged"], true);
    assert_eq!(again["allowed"], serde_json::json!(["repair"]));
    assert_eq!(again["retry_when"], "candidate_changed");
    assert_eq!(
        std::fs::read_to_string(&runs).unwrap().lines().count(),
        1,
        "no work was done"
    );

    // A repair is a new revision, and that does run.
    std::fs::write(view.join("src/new.rs"), "fn new() { repaired }\n").unwrap();
    let (code, repaired) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{repaired}");
    assert_ne!(repaired["detail"]["unchanged"], true);
    assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().count(), 2);
}

#[test]
fn an_unchanged_merge_conflict_is_refused_without_work() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/a.rs"), "fn a() { mine }\n").unwrap();
    // Trunk changes the same line.
    let other = test_box.clone_of(&remote, "other");
    std::fs::write(other.join("src/a.rs"), "fn a() { theirs }\n").unwrap();
    test_box.git_ok(&other, &["commit", "-q", "-am", "theirs"]);
    test_box.git_ok(&other, &["push", "-q", "origin", "main"]);

    let (code, first) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{first}");
    assert_eq!(first["reason"], "MERGE_CONFLICT");
    assert_eq!(
        first["detail"]["conflicts"],
        serde_json::json!(["src/a.rs"])
    );
    let (code, again) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{again}");
    assert_eq!(again["reason"], "MERGE_CONFLICT");
    assert_eq!(again["detail"]["unchanged"], true);
}

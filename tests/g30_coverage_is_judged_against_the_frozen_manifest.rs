//! G30. Coverage is judged against the frozen manifest.
//!
//! All checks green but a required check id missing: the aggregate is incomplete and
//! publication is blocked (a unit test of `evidence::aggregate`, and here a policy whose second
//! required check never reports); `status` never collapses the completion dimensions into
//! "done".

mod support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_missing_required_check_blocks_publication() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    // `tests` reports green; `lint` is required on another environment and its unit list is
    // empty, so it never reports a pass.
    let base = check_files(
        "[ \"$1\" = lint ] && { printf '%s' \"{\\\"schema\\\":\\\"bay.check.result/1\\\",\\\"exercised\\\":{\\\"commit\\\":\\\"$commit\\\",\\\"tree\\\":\\\"$tree\\\"},\\\"plan\\\":{\\\"selected\\\":0,\\\"completed\\\":0},\\\"checks\\\":[]}\"; exit 0; }",
        &passing_result(3),
        1,
        60,
    );
    let script = base
        .iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1
        .clone();
    let policy = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check tests\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 3\n\n[[check]]\nid = \"lint\"\nenvironment = \"arm\"\ncommand = \"scripts/bay-check lint\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n";
    let trunk = test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            ("scripts/bay-check".to_owned(), script),
            (".bay/policy.toml".to_owned(), policy.to_owned()),
        ],
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, blocked) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_UNKNOWN");
    assert_eq!(blocked["detail"]["checks"][0]["check"], "lint", "{blocked}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);

    let status = test_box.bay_ok(&repo, &["status", &id]);
    let completion = &status["detail"]["changes"][0]["completion"];
    assert_eq!(completion["published"], false, "{status}");
    assert_eq!(completion["checked"], false);
    assert!(!status.to_string().contains("\"done\""));
}

#[test]
fn a_landed_change_reports_each_dimension_apart() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0);
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let completion = &status["detail"]["changes"][0]["completion"];
    assert_eq!(completion["published"], true, "{status}");
    assert_eq!(completion["checked"], true);
    assert_eq!(completion["running"], "unconfirmed");
    assert_eq!(completion["effective"], "unconfirmed");
    assert!(!status.to_string().contains("\"done\""));
}

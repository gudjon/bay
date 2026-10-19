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

/// A trunk whose `lint` and `docs` checks fail every candidate (both red on trunk), plus a
/// passing `tests`; the policy ends with `extra`.
fn red_on_trunk(extra: &str) -> (TestBox, std::path::PathBuf, std::path::PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let script = check_files(
        &format!("{{ [ \"$1\" = lint ] || [ \"$1\" = docs ]; }} && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    )
    .into_iter()
    .find(|(path, _)| path == "scripts/bay-check")
    .unwrap()
    .1;
    let check = |id: &str| {
        format!(
            "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n"
        )
    };
    let policy = format!(
        "revision = 1\n{}{}{}{extra}",
        check("tests"),
        check("lint"),
        check("docs")
    );
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            ("scripts/bay-check".to_owned(), script),
            (".bay/policy.toml".to_owned(), policy),
        ],
    );
    (test_box, repo, remote)
}

const QUARANTINES: &str = "\n[[quarantine]]\nid = \"lint\"\nenvironment = \"local\"\nreason = \"red on trunk; fix in progress\"\nend = \"2020-01-01\"\n\n[[quarantine]]\nid = \"docs\"\nenvironment = \"local\"\nreason = \"red on trunk; fix in progress\"\nend = \"2999-01-01\"\n";

#[test]
fn a_quarantined_check_is_not_required_and_is_reported_as_a_coverage_gap() {
    let (test_box, repo, remote) = red_on_trunk(QUARANTINES);
    let trunk = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_ne!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
    // Each set-aside check is named as a coverage gap, never as tested.
    let gaps = landed["detail"]["coverage_gaps"].as_array().unwrap();
    let mut named: Vec<&str> = gaps
        .iter()
        .map(|gap| gap["check"].as_str().unwrap())
        .collect();
    named.sort_unstable();
    assert_eq!(named, ["docs", "lint"], "{landed}");
    let lint = gaps.iter().find(|gap| gap["check"] == "lint").unwrap();
    assert_eq!(lint["environment"], "local");
    assert_eq!(
        lint["quarantine"]["reason"], "red on trunk; fix in progress",
        "{landed}"
    );
    assert_eq!(lint["quarantine"]["end"], "2020-01-01");

    // `status` lists open quarantines; one past its end date stays in force, shown overdue.
    let status = test_box.bay_ok(&repo, &["status"]);
    let open = status["detail"]["quarantines"].as_array().unwrap();
    assert_eq!(open.len(), 2, "{status}");
    let overdue = |id: &str| {
        open.iter()
            .find(|entry| entry["check"] == id)
            .unwrap_or_else(|| panic!("{id} is not listed: {status}"))["overdue"]
            .clone()
    };
    assert_eq!(overdue("lint"), true, "{status}");
    assert_eq!(overdue("docs"), false, "{status}");

    // `bay doctor` shows the overdue quarantine too (DESIGN § 7), as a row that is not red.
    let (_, doctor) = result_json(test_box.bay(&repo).arg("doctor"));
    let row = doctor["detail"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "quarantine")
        .unwrap_or_else(|| panic!("no quarantine row: {doctor}"))
        .clone();
    assert_eq!(row["state"], "overdue", "{doctor}");
    assert_eq!(row["quarantines"].as_array().unwrap().len(), 2, "{doctor}");
}

#[test]
fn a_candidate_cannot_quarantine_the_check_it_fails() {
    // The candidate's own policy sets `lint` and `docs` aside, but it is judged by the base's
    // policy, which requires both: no candidate can quarantine the check it fails (DESIGN § 7).
    let (test_box, repo, remote) = red_on_trunk("");
    let trunk = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let started = test_box.bay_ok(&repo, &["start", "QUARANTINE-1", "--paths", ".bay/"]);
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let policy = std::fs::read_to_string(view.join(".bay/policy.toml")).unwrap();
    std::fs::write(
        view.join(".bay/policy.toml"),
        format!(
            "{}{QUARANTINES}",
            policy.replace("revision = 1", "revision = 2")
        ),
    )
    .unwrap();
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
}

/// One `[[check]]` entry on the shared adapter.
fn entry(id: &str, expected_minimum: u32) -> String {
    format!(
        "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = {expected_minimum}\n"
    )
}

/// A trunk under `policy` whose adapter passes one test for every check except `mapped`,
/// which it answers with a `declared_empty` selection.
fn mapped_trunk(policy: &str) -> (TestBox, std::path::PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let empty = "printf '%s' \"{\\\"schema\\\":\\\"bay.check.result/1\\\",\\\"exercised\\\":{\\\"commit\\\":\\\"$commit\\\",\\\"tree\\\":\\\"$tree\\\"},\\\"plan\\\":{\\\"selected\\\":0,\\\"completed\\\":0,\\\"selection\\\":\\\"declared_empty\\\"},\\\"checks\\\":[]}\"";
    let script = check_files(
        &format!("[ \"$1\" = mapped ] && {{ {empty}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    )
    .into_iter()
    .find(|(path, _)| path == "scripts/bay-check")
    .unwrap()
    .1;
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            ("scripts/bay-check".to_owned(), script),
            (".bay/policy.toml".to_owned(), policy.to_owned()),
        ],
    );
    (test_box, repo)
}

#[test]
fn a_policy_change_reports_the_gaps_of_its_own_policy_too() {
    let base = format!("revision = 1\n{}{}", entry("tests", 1), entry("more", 1));
    let (test_box, repo) = mapped_trunk(&base);
    // The candidate's own policy adds `mapped`, which passes on an empty selection, and sets
    // `more` aside. Both policies must pass (INTERFACES § 7): `mapped` passed untested under
    // the own policy, so it is a gap; the base's policy required `more` and it passed, so
    // `more` was tested and is no gap.
    let started = test_box.bay_ok(&repo, &["start", "POLICY-2", "--paths", ".bay/"]);
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(
        view.join(".bay/policy.toml"),
        format!(
            "revision = 2\n{}{}{}\n[[quarantine]]\nid = \"more\"\nenvironment = \"local\"\nreason = \"flaky on arm\"\nend = \"2999-01-01\"\n",
            entry("tests", 1),
            entry("more", 1),
            entry("mapped", 0)
        ),
    )
    .unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(
        landed["detail"]["coverage_gaps"],
        serde_json::json!([{"check": "mapped", "environment": "local", "selection": "declared_empty"}]),
        "{landed}"
    );
}

#[test]
fn a_quarantine_that_names_no_check_or_has_no_valid_end_makes_the_policy_invalid() {
    // A typo in a quarantine would set nothing aside, or never show as overdue: never silent.
    for (id, end) in [("lnit", "2999-01-01"), ("tests", "next week")] {
        let policy = format!(
            "revision = 1\n{}{}\n[[quarantine]]\nid = \"{id}\"\nenvironment = \"local\"\nreason = \"red on trunk\"\nend = \"{end}\"\n",
            entry("tests", 1),
            entry("more", 1)
        );
        let (test_box, repo) = mapped_trunk(&policy);
        let (_, view) = test_box.start_and_edit(&repo);
        let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
        assert_eq!(code, 10, "{id} {end}: {refused}");
        assert_eq!(refused["reason"], "POLICY_INVALID", "{id} {end}: {refused}");
    }
}

#[test]
fn a_quarantined_check_does_not_count_toward_the_above_zero_rule() {
    // The only check with a minimum above zero is set aside, so every required check may
    // collect nothing: the policy is invalid (INTERFACES § 7, ruling 8c).
    let policy = format!(
        "revision = 1\n{}{}\n[[quarantine]]\nid = \"tests\"\nenvironment = \"local\"\nreason = \"red on trunk\"\nend = \"2999-01-01\"\n",
        entry("tests", 1),
        entry("mapped", 0)
    );
    let (test_box, repo) = mapped_trunk(&policy);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
}

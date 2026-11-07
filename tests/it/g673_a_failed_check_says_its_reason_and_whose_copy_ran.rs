//! G673 (a move of 4,154 files, 2026-11-07: twice refused "a check did not pass", the reason only
//! in `evidence.records[].reasons`; then refused again because its own edit of the check was not
//! what ran). The sentence of a `CHECK_FAILED`, at `bay merge` and in `bay status`, names the
//! failed unit and the first line of its reason. A change that edits a file the failed check runs
//! from hears that the check ran as main has it (G26), so that edit lands first, on its own.
//!
//! Guards to mutate, in `src/result.rs`: `failed_unit` returning `None` (the sentence names no
//! reason); `status_say` passing no detail for `CHECK_FAILED` (status names none). In
//! `src/publish.rs`: the `runs_as_main` paths left empty (the change does not hear whose copy ran).

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, result_json};

fn failing_result(reason: &str) -> String {
    format!(
        "printf '%s' \"{{\\\"schema\\\":\\\"bay.check.result/1\\\",\\\"exercised\\\":{{\\\"commit\\\":\\\"$commit\\\",\\\"tree\\\":\\\"$tree\\\"}},\\\"plan\\\":{{\\\"discovered\\\":2,\\\"selected\\\":2,\\\"completed\\\":2}},\\\"checks\\\":[{{\\\"id\\\":\\\"t1\\\",\\\"verdict\\\":\\\"pass\\\"}},{{\\\"id\\\":\\\"t2\\\",\\\"verdict\\\":\\\"product_failure\\\",\\\"reason\\\":\\\"{reason}\\\"}}]}}\""
    )
}

const REASON: &str =
    r"...a cut line\\n  [dangling] src/a.rs: cites docs/old.md which does not exist.\\nmore";

#[test]
fn the_refusal_and_status_say_the_failed_unit_and_its_first_line() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("", &failing_result(REASON), 2, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);

    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    let said = refused["say"].as_str().unwrap();
    assert!(
        said.contains(
            "(tests) did not pass: t2: [dangling] src/a.rs: cites docs/old.md which does not exist."
        ),
        "{said}"
    );
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let said = status["say"].as_str().unwrap();
    assert!(
        said.contains("t2: [dangling] src/a.rs: cites docs/old.md"),
        "{said}"
    );
}

#[test]
fn a_change_to_the_failed_checks_own_file_hears_main_ran_it() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("", &failing_result("old rule"), 2, 60),
    );
    let started = test_box.bay_ok(
        &repo,
        &["start", "new rule", "--paths", "scripts/", "src/", ".bay/"],
    );
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let script = view.join("scripts/bay-check");
    let text = std::fs::read_to_string(&script).unwrap();
    std::fs::write(&script, text.replace("old rule", "new rule")).unwrap();
    std::fs::write(view.join("src/a.rs"), "fn a() { }\n").unwrap();
    // An edit of a check's own file raises the policy's revision (G26).
    let policy = view.join(".bay/policy.toml");
    let rules = std::fs::read_to_string(&policy).unwrap();
    std::fs::write(&policy, rules.replace("revision = 1", "revision = 2")).unwrap();

    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    assert_eq!(
        refused["detail"]["checks"][0]["runs_as_main"],
        serde_json::json!(["scripts/bay-check"]),
        "{refused}"
    );
    let said = refused["say"].as_str().unwrap();
    assert!(said.contains("ran as main has it"), "{said}");
    assert!(said.contains("scripts/bay-check"), "{said}");
}

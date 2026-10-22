//! G259. A flaky check is retried on the same tree and named, never a plain pass.
//!
//! `retry_on_failure = N` in a `[[check]]` runs a check that answers `product_failure` again on
//! the identical tree, up to N times. Every run is its own record on the tree, so a failure and
//! a later pass combine to `flaky` (`DESIGN` § 7), never to `pass`. The landing proceeds only
//! when the entry says `allow_flaky = true`, and then the land result names the check under
//! `coverage_gaps` with every run; otherwise it is refused `CHECK_FLAKY`, as before. Without
//! `retry_on_failure` a failure runs once. `bay status` counts each check's flaky landings over
//! the window under `detail.flakes`, and lists each quarantine with its `owner`, its age and
//! whether it is overdue. An overdue quarantine stays in force (`DESIGN` § 7: an end never
//! changes a judgement by itself).

// Each case runs a check adapter (a shell script), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// The adapter answer of one `product_failure` unit.
fn failing_result() -> String {
    passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"")
}

/// A trunk whose one required check `tests` answers `product_failure` on its first `fails`
/// runs and passes after; `extra` is appended to its `[[check]]` table. Every run appends a
/// line to the returned run log, through the relay: the check sees nothing of the box.
fn flaky_trunk(test_box: &TestBox, fails: u32, extra: &str) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let runs = test_box.path().join("runs.log");
    let before = format!(
        "if {counted}then {pass}; else {fail}; fi; exit 0",
        // Logs this run and answers whether it comes after the first `fails` runs.
        counted = test_box.relay(&format!(
            "echo run >> '{log}'; [ $(wc -l < '{log}') -gt {fails} ]",
            log = runs.display()
        )),
        pass = passing_result(1),
        fail = failing_result(),
    );
    let files: Vec<(String, String)> = check_files(&before, "true", 1, 60)
        .into_iter()
        .map(|(path, text)| {
            if path == ".bay/policy.toml" {
                (path, format!("{text}{extra}"))
            } else {
                (path, text)
            }
        })
        .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (repo, runs)
}

/// How many runs the check made.
fn runs(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .count()
}

/// The records of check `tests` on `tree` in the remote, as (id, verdict), sorted by id.
fn records_on(test_box: &TestBox, remote: &Path, tree: &str) -> Vec<(String, String)> {
    let notes = test_box.git_bare(
        remote,
        &["notes", "--ref=refs/notes/bay/evidence/local", "show", tree],
    );
    let records: Vec<Value> = serde_json::from_str(&notes).unwrap();
    let mut out: Vec<(String, String)> = records
        .iter()
        .filter(|record| record["check"]["id"] == "tests")
        .map(|record| {
            (
                record["id"].as_str().unwrap().to_owned(),
                record["verdict"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn a_check_that_fails_once_then_passes_lands_flaky_with_both_runs_when_allowed() {
    let test_box = TestBox::new();
    let (repo, log) = flaky_trunk(&test_box, 1, "retry_on_failure = 1\nallow_flaky = true\n");
    let remote = test_box.path().join("repo-remote.git");
    let trunk = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_ne!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
    assert_eq!(runs(&log), 2, "one failure, one rerun");

    // Both runs are on the landed tree, each its own record.
    let tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    let records = records_on(&test_box, &remote, &tree);
    let verdicts: Vec<&str> = records
        .iter()
        .map(|(_, verdict)| verdict.as_str())
        .collect();
    assert_eq!(verdicts.len(), 2, "{records:?}");
    assert!(verdicts.contains(&"product_failure"), "{records:?}");
    assert!(verdicts.contains(&"pass"), "{records:?}");
    assert_ne!(
        records[0].0, records[1].0,
        "two runs share an id: {records:?}"
    );

    // The landing names the check flaky with every run, never as a plain pass.
    let gaps = landed["detail"]["coverage_gaps"]
        .as_array()
        .unwrap_or_else(|| panic!("no coverage_gaps: {landed}"));
    let flaky = gaps
        .iter()
        .find(|gap| gap["check"] == "tests")
        .unwrap_or_else(|| panic!("tests is not named flaky: {landed}"));
    assert_eq!(flaky["flaky"]["allowed"], true, "{landed}");
    assert_eq!(
        flaky["flaky"]["runs"].as_array().map(Vec::len),
        Some(2),
        "{landed}"
    );

    // `bay status` counts the flaky landing.
    let status = test_box.bay_ok(&repo, &["status"]);
    let row = status["detail"]["flakes"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["check"] == "tests"))
        .unwrap_or_else(|| panic!("no flake row for tests: {status}"));
    assert_eq!(row["flaky"], 1, "{status}");
    assert_eq!(row["retry_on_failure"], 1, "{status}");
    assert_eq!(row["allow_flaky"], true, "{status}");
}

#[test]
fn a_check_that_fails_once_then_passes_is_refused_check_flaky_unless_allowed() {
    let test_box = TestBox::new();
    let (repo, log) = flaky_trunk(&test_box, 1, "retry_on_failure = 1\n");
    let remote = test_box.path().join("repo-remote.git");
    let trunk = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FLAKY", "{refused}");
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        trunk,
        "a flaky tree published: {refused}"
    );
    assert_eq!(runs(&log), 2, "one failure, one rerun");
    // The refusal names both runs.
    let checks = refused["detail"]["checks"].as_array().unwrap();
    let mut verdicts: Vec<&str> = checks
        .iter()
        .filter(|check| check["check"] == "tests")
        .map(|check| check["verdict"].as_str().unwrap())
        .collect();
    verdicts.sort_unstable();
    assert_eq!(verdicts, ["pass", "product_failure"], "{refused}");
}

#[test]
fn without_retry_on_failure_a_failure_runs_once() {
    let test_box = TestBox::new();
    let (repo, log) = flaky_trunk(&test_box, 1, "allow_flaky = true\n");
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    assert_eq!(
        runs(&log),
        1,
        "a failure was rerun with no retry_on_failure"
    );
}

#[test]
fn a_check_that_fails_every_run_is_rerun_at_most_n_times_and_fails() {
    let test_box = TestBox::new();
    let (repo, log) = flaky_trunk(&test_box, 99, "retry_on_failure = 2\nallow_flaky = true\n");
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    assert_eq!(runs(&log), 3, "one run and two reruns");
}

#[test]
fn a_flake_key_out_of_bounds_or_on_a_post_check_makes_the_policy_invalid() {
    for extra in [
        "retry_on_failure = 6\n",
        "stage = \"post\"\nallow_flaky = true\n",
    ] {
        let test_box = TestBox::new();
        // A post check is never required, so a second pre check keeps the policy otherwise valid.
        let extra = format!(
            "{extra}\n[[check]]\nid = \"more\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n"
        );
        let (repo, _) = flaky_trunk(&test_box, 0, &extra);
        let (_, view) = test_box.start_and_edit(&repo);
        let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 10, "{extra}: {refused}");
        assert_eq!(refused["reason"], "POLICY_INVALID", "{extra}: {refused}");
    }
}

#[test]
fn an_overdue_quarantine_stays_in_force_and_status_names_its_owner_and_age() {
    let test_box = TestBox::new();
    // `lint` fails every run and is set aside past its end; `tests` passes.
    let quarantine = "\n[[check]]\nid = \"lint\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check lint\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n\n[[quarantine]]\nid = \"lint\"\nenvironment = \"local\"\nreason = \"red on trunk\"\nend = \"2020-01-31\"\nowner = \"platform-team\"\nsince = \"2020-01-01\"\n";
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files(
        &format!("[ \"$1\" = lint ] && {{ {}; exit 0; }}", failing_result()),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path.as_str() == ".bay/policy.toml" {
            text.push_str(quarantine);
        }
    }
    let trunk = test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    // DESIGN § 7: a quarantine past its end stays in force until a policy change removes it.
    assert_eq!(code, 0, "{landed}");
    assert_ne!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
    let gap = landed["detail"]["coverage_gaps"]
        .as_array()
        .and_then(|gaps| gaps.iter().find(|gap| gap["check"] == "lint"))
        .unwrap_or_else(|| panic!("lint is not named a gap: {landed}"));
    assert_eq!(gap["quarantine"]["owner"], "platform-team", "{landed}");

    let status = test_box.bay_ok(&repo, &["status"]);
    let open = status["detail"]["quarantines"].as_array().unwrap();
    assert_eq!(open.len(), 1, "{status}");
    assert_eq!(open[0]["check"], "lint", "{status}");
    assert_eq!(open[0]["owner"], "platform-team", "{status}");
    assert_eq!(open[0]["since"], "2020-01-01", "{status}");
    assert_eq!(open[0]["overdue"], true, "{status}");
    let age = open[0]["age_days"].as_i64().unwrap_or_default();
    assert!(age > 365 * 6, "{status}");
}

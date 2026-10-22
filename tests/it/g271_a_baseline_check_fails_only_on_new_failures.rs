//! G271 (N-3). A check with `baseline = "base"` fails a candidate only on new failures.
//!
//! With `baseline = "base"` in its `[[check]]`, a unit that fails on the candidate is a failure
//! only when the same unit (by id) passed on the base, or the base has no such unit. A unit
//! that fails on both is pre-existing: it does not fail the candidate, and the landing names it
//! under `coverage_gaps` as `baseline_failures`, never as a pass. The base's result comes from
//! evidence: the base's own record of the check under the same manifest, else one run of the
//! check on the base, recorded on the base's tree as its evidence and reused by later
//! candidates. A base run that is `unknown` or `infra_failure` makes the candidate unknown.
//! Without `baseline`, any failing unit fails the check, as before. The candidate's record
//! names the base record it was compared with (`baseline: {commit, record, failures}`).
//!
//! The guards this test protects; mutate each and watch a case fail:
//! - `src/check.rs`, `against_base`: `if preexisting.len() == failing.len()` (make it `true`:
//!   a candidate with a new failing unit `v` lands).
//! - `src/check.rs`, `against_base`: `if matches!(record.verdict, Verdict::Unknown |
//!   Verdict::InfraFailure)` (make it `false`: a base `infra_failure` turns into
//!   `CHECK_FAILED`, not `CHECK_UNKNOWN`).
//! - `src/check.rs`, `run_all`: `entry.baseline.is_some() && judged.verdict ==
//!   Verdict::ProductFailure` (drop `entry.baseline.is_some()`: a check without `baseline`
//!   lands over a failing unit).
//! - `src/publish.rs`, `gap_row`: `.filter(|unit| !preexisting.contains(&unit.id))` (drop it:
//!   the refusal names the pre-existing `u` beside `v`).
//! - `src/publish.rs`, `coverage_gaps`: the `baseline_gaps` loop (drop it: the landing no
//!   longer names `u` under `baseline_failures`).
//! - `src/check.rs`, `base_answer`: the `answers_on_base` lookup (drop it: the second
//!   candidate runs the check on a base that already has its own record).

// Each case runs a shell adapter, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, result_json};

/// A shell line that prints a `bay.check.result/1` with `units`, each (id, verdict); a unit
/// that does not pass says why (`<id> broke`).
fn result_of(units: &[(&str, &str)]) -> String {
    let count = units.len();
    let checks: Vec<String> = units
        .iter()
        .map(|(id, verdict)| {
            let reason = if *verdict == "pass" {
                String::new()
            } else {
                format!(",\\\"reason\\\":\\\"{id} broke\\\"")
            };
            format!("{{\\\"id\\\":\\\"{id}\\\",\\\"verdict\\\":\\\"{verdict}\\\"{reason}}}")
        })
        .collect();
    format!(
        "printf '%s' \"{{\\\"schema\\\":\\\"bay.check.result/1\\\",\\\"exercised\\\":{{\\\"commit\\\":\\\"$commit\\\",\\\"tree\\\":\\\"$tree\\\"}},\\\"plan\\\":{{\\\"discovered\\\":{count},\\\"selected\\\":{count},\\\"completed\\\":{count}}},\\\"checks\\\":[{}]}}\"",
        checks.join(",")
    )
}

/// A trunk whose one required check `tests` answers `on_base` on a tree without `src/new.rs`
/// (the seeded trunk) and `on_candidate` on a tree with it (a change from
/// [`TestBox::start_and_edit`]); `extra` is appended to its `[[check]]` table.
fn trunk(
    test_box: &TestBox,
    on_base: &[(&str, &str)],
    on_candidate: &[(&str, &str)],
    extra: &str,
) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let before = format!(
        "if [ -f src/new.rs ]; then {}; else {}; fi; exit 0",
        result_of(on_candidate),
        result_of(on_base)
    );
    let files: Vec<(String, String)> = check_files(&before, "true", 2, 60)
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
    (repo, test_box.path().join("repo-remote.git"))
}

/// The records on `tree` in the remote, of this box's producer.
fn records_on(test_box: &TestBox, remote: &Path, tree: &str) -> Vec<Value> {
    let notes = test_box.git_bare(
        remote,
        &["notes", "--ref=refs/notes/bay/evidence/local", "show", tree],
    );
    serde_json::from_str(&notes).unwrap()
}

/// The `tests` record of the landed tip whose verdict is `pass`.
fn landed_record(test_box: &TestBox, remote: &Path) -> Value {
    let tree = test_box.git_bare(remote, &["rev-parse", "main^{tree}"]);
    records_on(test_box, remote, &tree)
        .into_iter()
        .find(|record| record["check"]["id"] == "tests" && record["verdict"] == "pass")
        .unwrap_or_else(|| panic!("no passing tests record on the landed tree {tree}"))
}

const BASELINE: &str = "baseline = \"base\"\n";

#[test]
fn a_unit_failing_on_base_and_candidate_lands_named_under_baseline_failures() {
    let test_box = TestBox::new();
    let failing = [("t", "pass"), ("u", "product_failure")];
    let (repo, remote) = trunk(&test_box, &failing, &failing, BASELINE);
    let seed = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let seed_tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    // A second change, started on the same trunk, lands after the first: its base is then the
    // first's landing, whose own record answers for that base.
    let (_, first) = test_box.start_and_edit(&repo);
    let started = test_box.bay_ok(&repo, &["start", "BASELINE-2", "--paths", "src/"]);
    let second = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(second.join("src/other.rs"), "fn other() {}\n").unwrap();

    let (code, landed) = result_json(test_box.bay(&first).args(["merge", "--wait"]));
    assert_eq!(
        code, 0,
        "a pre-existing failure failed the candidate: {landed}"
    );
    assert_eq!(landed["decision"], "merged", "{landed}");
    let gap = landed["detail"]["coverage_gaps"]
        .as_array()
        .and_then(|gaps| gaps.iter().find(|gap| gap["check"] == "tests"))
        .unwrap_or_else(|| panic!("tests is not named a coverage gap: {landed}"));
    assert_eq!(
        gap["baseline_failures"],
        serde_json::json!(["u"]),
        "{landed}"
    );

    // The base was run once, as the base's own evidence on its tree, and the candidate's
    // record names that record.
    let on_seed = records_on(&test_box, &remote, &seed_tree);
    let base_runs: Vec<&Value> = on_seed
        .iter()
        .filter(|record| record["check"]["id"] == "tests")
        .collect();
    assert_eq!(base_runs.len(), 1, "one run on the base: {on_seed:?}");
    assert_eq!(base_runs[0]["subject"]["commit"], seed.as_str());
    assert_eq!(base_runs[0]["verdict"], "product_failure");
    let record = landed_record(&test_box, &remote);
    assert_eq!(record["baseline"]["commit"], seed.as_str(), "{record}");
    assert_eq!(record["baseline"]["record"], base_runs[0]["id"], "{record}");
    assert_eq!(record["baseline"]["failures"], serde_json::json!(["u"]));
    let units: Vec<(&Value, &Value)> = record["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|unit| (&unit["id"], &unit["verdict"]))
        .collect();
    assert!(
        units.contains(&(&Value::from("u"), &Value::from("product_failure"))),
        "the record keeps the unit's real verdict: {record}"
    );

    // The second candidate's base is the first landing: its own record answers, so no run is
    // made on it.
    let first_landing = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let first_tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    let first_record = record["id"].clone();
    let (code, landed) = result_json(test_box.bay(&second).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let on_first = records_on(&test_box, &remote, &first_tree);
    assert!(
        on_first
            .iter()
            .all(|record| !record["id"].as_str().unwrap().contains("-baseline-")),
        "the base's own record was not reused: {on_first:?}"
    );
    let record = landed_record(&test_box, &remote);
    assert_eq!(record["baseline"]["commit"], first_landing.as_str());
    assert_eq!(record["baseline"]["record"], first_record, "{record}");
}

#[test]
fn a_unit_failing_only_on_the_candidate_is_check_failed_naming_only_it() {
    let test_box = TestBox::new();
    let (repo, remote) = trunk(
        &test_box,
        &[("t", "pass"), ("u", "product_failure")],
        &[
            ("t", "pass"),
            ("u", "product_failure"),
            ("v", "product_failure"),
        ],
        BASELINE,
    );
    let seed = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), seed);
    let check = refused["detail"]["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|check| check["check"] == "tests"))
        .unwrap_or_else(|| panic!("tests is not named: {refused}"));
    assert_eq!(
        check["reasons"],
        serde_json::json!([{"unit": "v", "verdict": "product_failure", "reason": "v broke"}]),
        "only the new failure is named: {refused}"
    );
    assert_eq!(
        check["baseline_failures"],
        serde_json::json!(["u"]),
        "{refused}"
    );
}

#[test]
fn without_baseline_a_failing_unit_is_check_failed() {
    let test_box = TestBox::new();
    let failing = [("t", "pass"), ("u", "product_failure")];
    let (repo, remote) = trunk(&test_box, &failing, &failing, "");
    let seed = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), seed);
    let check = &refused["detail"]["checks"][0];
    assert_eq!(
        check["reasons"],
        serde_json::json!([{"unit": "u", "verdict": "product_failure", "reason": "u broke"}]),
        "{refused}"
    );
    // No base was run: the seeded trunk's tree holds no record.
    let seed_tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    let git_dir = format!("--git-dir={}", remote.display());
    let out = test_box
        .git(test_box.path())
        .args([
            git_dir.as_str(),
            "notes",
            "--ref=refs/notes/bay/evidence/local",
            "show",
            seed_tree.as_str(),
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a check without baseline ran on the base: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn an_infra_failure_on_the_base_makes_the_candidate_unknown() {
    let test_box = TestBox::new();
    let (repo, remote) = trunk(
        &test_box,
        &[("t", "pass"), ("u", "infra_failure")],
        &[("t", "pass"), ("u", "product_failure")],
        BASELINE,
    );
    let seed = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_ne!(code, 0, "{refused}");
    assert_eq!(refused["reason"], "CHECK_UNKNOWN", "{refused}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), seed);
    let check = refused["detail"]["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|check| check["check"] == "tests"))
        .unwrap_or_else(|| panic!("tests is not named: {refused}"));
    assert_eq!(check["verdict"], "unknown", "{refused}");
    assert!(
        check["why"]
            .as_str()
            .is_some_and(|why| why.contains("base") && why.contains("infra_failure")),
        "the refusal says the base's run is no answer: {refused}"
    );
}

#[test]
fn a_baseline_other_than_base_makes_the_policy_invalid() {
    let test_box = TestBox::new();
    let failing = [("t", "pass"), ("u", "product_failure")];
    let (repo, _) = trunk(&test_box, &failing, &failing, "baseline = \"trunk\"\n");
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
}

//! G3. Unknown is never pass.
//!
//! A required check is missing, crashes, times out or collects nothing: land stays blocked
//! with `CHECK_UNKNOWN`, and trunk does not move.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

fn blocked_with(
    before: &str,
    result: &str,
    expected_minimum: u32,
    timeout_s: u32,
) -> serde_json::Value {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let trunk = test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(before, result, expected_minimum, timeout_s),
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        trunk,
        "trunk moved"
    );
    result
}

#[test]
fn a_check_that_collects_nothing_is_unknown() {
    let result = blocked_with("true", &passing_result(0), 1, 60);
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
    assert_eq!(result["allowed"], serde_json::json!(["wait", "repair"]));
}

#[test]
fn fewer_than_expected_minimum_is_unknown() {
    let result = blocked_with("true", &passing_result(2), 3, 60);
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
    assert!(
        result.to_string().contains("expected_minimum 3"),
        "{result}"
    );
}

#[test]
fn a_check_that_crashes_is_unknown() {
    let result = blocked_with("exit 3", "true", 1, 60);
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
}

#[test]
fn a_check_that_times_out_is_unknown() {
    let result = blocked_with("sleep 5", &passing_result(1), 1, 1);
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
    assert!(result.to_string().contains("timed out"), "{result}");
}

#[test]
fn an_exit_code_alone_is_not_evidence() {
    let result = blocked_with("echo all good; exit 0", "true", 1, 60);
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
}

#[test]
fn a_result_for_another_commit_is_rejected_whole() {
    let result = blocked_with(
        "commit=0000000000000000000000000000000000000000",
        &passing_result(1),
        1,
        60,
    );
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
    assert!(result.to_string().contains("different commit"), "{result}");
}

#[test]
fn a_product_failure_is_check_failed() {
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let result = blocked_with("true", &failing, 1, 60);
    assert_eq!(result["reason"], "CHECK_FAILED");
}

/// A result for check `mapped`: `units` passing tests, `selected` as given, and, when
/// `declared`, `"selection": "declared_empty"` in its plan.
fn mapped_result(units: u32, selected: u32, declared: bool) -> String {
    let checks: Vec<String> = (1..=units)
        .map(|n| format!("{{\\\"id\\\":\\\"m{n}\\\",\\\"verdict\\\":\\\"pass\\\"}}"))
        .collect();
    let selection = if declared {
        ",\\\"selection\\\":\\\"declared_empty\\\""
    } else {
        ""
    };
    format!(
        "printf '%s' \"{{\\\"schema\\\":\\\"bay.check.result/1\\\",\\\"exercised\\\":{{\\\"commit\\\":\\\"$commit\\\",\\\"tree\\\":\\\"$tree\\\"}},\\\"plan\\\":{{\\\"selected\\\":{selected},\\\"completed\\\":{selected}{selection}}},\\\"checks\\\":[{}]}}\"",
        checks.join(",")
    )
}

/// Lands a change under a policy of two checks: `tests` (one passing test, `expected_minimum`
/// `tests_minimum`) and `mapped` (`expected_minimum` `mapped_minimum`, answering `mapped`).
/// Returns the land's exit code and result, and whether trunk moved.
fn land_with_mapped(
    tests_minimum: u32,
    mapped_minimum: u32,
    mapped: &str,
) -> (i32, serde_json::Value, bool) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let base = check_files(
        &format!("[ \"$1\" = mapped ] && {{ {mapped}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    );
    let script = base
        .iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1
        .clone();
    let policy = format!(
        "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check tests\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = {tests_minimum}\n\n[[check]]\nid = \"mapped\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check mapped\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = {mapped_minimum}\n"
    );
    let trunk = test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            ("scripts/bay-check".to_owned(), script),
            (".bay/policy.toml".to_owned(), policy),
        ],
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    let moved = test_box.git_bare(&remote, &["rev-parse", "main"]) != trunk;
    (code, result, moved)
}

#[test]
fn an_explicit_zero_minimum_passes_an_empty_selection_as_a_coverage_gap() {
    let (code, result, moved) = land_with_mapped(1, 0, &mapped_result(0, 0, true));
    assert_eq!(code, 0, "{result}");
    assert!(moved, "trunk did not move: {result}");
    let gaps = result["detail"]["coverage_gaps"].as_array().unwrap();
    assert_eq!(gaps.len(), 1, "{result}");
    assert_eq!(gaps[0]["check"], "mapped", "{result}");
    assert_eq!(gaps[0]["environment"], "local", "{result}");
    assert_eq!(gaps[0]["selection"], "declared_empty", "{result}");
}

#[test]
fn declared_empty_with_a_selection_is_unknown() {
    let (code, result, moved) = land_with_mapped(1, 0, &mapped_result(2, 2, true));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
    assert!(result.to_string().contains("declared_empty"), "{result}");
    assert!(!moved, "trunk moved");
}

#[test]
fn a_selection_with_no_unit_reported_is_unknown_never_declared_empty() {
    // Undeclared: the adapter says it selected and completed two tests but reports none.
    // Under an explicit zero minimum that is still no empty selection.
    let (code, result, moved) = land_with_mapped(1, 0, &mapped_result(0, 2, false));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "CHECK_UNKNOWN", "{result}");
    assert!(!moved, "trunk moved");
}

#[test]
fn an_undeclared_empty_selection_under_a_zero_minimum_is_recorded_as_declared_empty() {
    // bay records `declared_empty` itself when the adapter does not, so an empty pass is
    // never shown as tested code.
    let (code, result, moved) = land_with_mapped(1, 0, &mapped_result(0, 0, false));
    assert_eq!(code, 0, "{result}");
    assert!(moved, "trunk did not move: {result}");
    assert_eq!(
        result["detail"]["coverage_gaps"][0]["selection"], "declared_empty",
        "{result}"
    );
}

#[test]
fn declared_empty_with_a_minimum_above_zero_is_unknown() {
    // The adapter reports one passing test but declares the selection empty under a minimum
    // of one: no combination but selected = 0 and expected_minimum = 0 is valid.
    let (code, result, moved) = land_with_mapped(1, 1, &mapped_result(1, 0, true));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "CHECK_UNKNOWN");
    assert!(result.to_string().contains("declared_empty"), "{result}");
    assert!(!moved, "trunk moved");
}

#[test]
fn a_policy_with_no_check_above_zero_is_policy_invalid() {
    let (code, result, moved) = land_with_mapped(0, 0, &mapped_result(0, 0, true));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "POLICY_INVALID", "{result}");
    assert_eq!(result["allowed"], serde_json::json!(["repair"]));
    assert_eq!(result["retry_when"], "policy_repaired");
    assert!(!moved, "trunk moved");
}

/// A required check that is missing: its adapter never starts (its interpreter does not
/// exist, so the process cannot be spawned). There is no result, and no result is never pass.
#[test]
fn a_check_that_never_starts_is_unknown() {
    let test_box = TestBox::new();
    // On Linux, no sandbox wrapper: bay spawns the adapter itself and the spawn fails. A new
    // test box chose to run checks unsandboxed.
    test_box.no_sandbox();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files: Vec<(String, String)> = check_files("true", &passing_result(1), 1, 60)
        .into_iter()
        .map(|(path, text)| {
            let text = if path == "scripts/bay-check" {
                text.replacen("#!/bin/sh\n", "#!/nonexistent/bay-no-such-shell\n", 1)
            } else {
                text
            };
            (path, text)
        })
        .collect();
    let trunk = test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "CHECK_UNKNOWN", "{result}");
    assert_eq!(
        result["detail"]["checks"][0]["verdict"], "unknown",
        "{result}"
    );
    if cfg!(target_os = "linux") {
        assert!(result.to_string().contains("did not start"), "{result}");
    }
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        trunk,
        "trunk moved"
    );
}

/// A required check with no result that counts: the check ran and passed, but its record is
/// signed by a producer key the registry does not hold under that producer's name, so no
/// counted record answers the required id. A required id with no result is never pass.
#[test]
fn a_required_check_with_no_counted_result_is_unknown() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let trunk = test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let keys = test_box.keys();
    // The producer name this box signs under is enrolled with another key.
    test_box.enroll_classes(
        &repo,
        &keys,
        &keys.executor,
        &[],
        &[(support::PRODUCER, &keys.rogue)],
    );
    test_box.use_keys(&repo, &keys);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "CHECK_UNKNOWN", "{result}");
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        trunk,
        "trunk moved"
    );
}

/// A required pair that no record answers at all: the policy requires `tests` in `local` and
/// in `env-b`, and this box declares only `local` (`bay.checkEnvironment`). Nothing runs for the
/// pair it cannot serve, so no record names it, and a required check with no record is never
/// pass: the change waits for a worker with `CHECK_UNAVAILABLE`, and trunk does not move.
#[test]
fn a_required_pair_this_box_does_not_serve_is_never_pass() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files: Vec<(String, String)> = check_files("true", &passing_result(1), 1, 60)
        .into_iter()
        .map(|(path, text)| {
            if path != ".bay/policy.toml" {
                return (path, text);
            }
            let second = text
                .split_once("[[check]]")
                .map(|(_, check)| check.replace("\"local\"", "\"env-b\""))
                .unwrap();
            (path, format!("{text}\n[[check]]{second}"))
        })
        .collect();
    let trunk = test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    test_box.git_ok(
        test_box.path(),
        &["config", "--global", "bay.checkEnvironment", "local"],
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "queued", "{result}");
    assert_eq!(result["reason"], "CHECK_UNAVAILABLE", "{result}");
    assert_eq!(
        result["detail"]["environments"],
        serde_json::json!(["env-b"]),
        "{result}"
    );
    assert_ne!(result["published"], true, "{result}");
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        trunk,
        "trunk moved"
    );
}

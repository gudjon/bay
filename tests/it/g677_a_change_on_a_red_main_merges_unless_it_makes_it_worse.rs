//! G677 (owner ruling 2026-11-07, "merge unless worse": in one day a red post check held 31 changes
//! for 19 hours in all, more than every refusal together). A change on the paths of a red post
//! check no longer waits for main: that check runs on the change before it merges, judged against
//! main as a `baseline = "base"` check is, and only a failure main does not have refuses it.
//!
//! Guards to mutate, in `src/publish.rs` `red_gate`: waiting in line again (the first case waits);
//! `check_candidate` judging with the policy as loaded, not `judging_red` (the second case merges).
//! In `src/policy.rs` `judging_red`: no baseline on the moved check (the first case fails on main's
//! own failure). In `src/publish.rs` `answers_of`: a run that passed against its base read as a
//! pass (main reads green after the first case's merge).

// Each case runs a shell script (the check's adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// Main red on `suite` (it fails one unit while `src/bad.rs` exists, two while `src/worse.rs` does).
fn red_main() -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let fail = |units| passing_result(units).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let (one, two) = (fail(1), fail(2));
    let mut files = check_files(
        &format!(
            "[ \"$1\" = suite ] && [ -e src/worse.rs ] && {{ {two}; exit 0; }}; \
             [ \"$1\" = suite ] && [ -e src/bad.rs ] && {{ {one}; exit 0; }}"
        ),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            *text = text.replacen("timeout_s", "when_paths = [\"src/**\"]\ntimeout_s", 1);
            text.push_str(POST);
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let view = edited(&test_box, &repo, "BAD", "src/bad.rs");
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(landed["decision"], "merged", "{code} {landed}");
    let out = test_box
        .bay(&repo)
        .args(["--json", "runner", "run", "--once"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    (test_box, repo)
}

fn edited(test_box: &TestBox, repo: &Path, intent: &str, file: &str) -> PathBuf {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), "x\n").unwrap();
    view
}

#[test]
fn a_change_on_a_red_checks_paths_merges_when_it_adds_no_failure() {
    let (test_box, repo) = red_main();
    let view = edited(&test_box, &repo, "OTHER", "src/c.rs");

    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    assert_eq!(
        merged["detail"]["red_checked"],
        serde_json::json!(["suite"]),
        "{merged}"
    );
    // Its own run passed only by main's own failure, so main is still red on `suite`.
    let red = test_box.bay_ok(&repo, &["status"])["detail"]["red"].clone();
    assert_eq!(
        red[0]["check"], "suite",
        "main reads green after a merge on its red: {red}"
    );
}

#[test]
fn a_change_that_adds_a_failure_to_a_red_check_is_refused() {
    let (test_box, repo) = red_main();
    let view = edited(&test_box, &repo, "WORSE", "src/worse.rs");

    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    let check = &refused["detail"]["checks"][0];
    assert_eq!(check["check"], "suite", "{refused}");
    // Main's own failure is named as pre-existing; the refusal is for the new unit.
    assert_eq!(
        check["baseline_failures"],
        serde_json::json!(["t1"]),
        "{refused}"
    );
}

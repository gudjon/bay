//! G815 (a fleet finding, 2026-11-20: a change on a red post check's paths ran that check before
//! its merge on the units its own diff selected, none of the 25 that failed on main; its pass was
//! recorded on the tree that landed, and cleared the red). The run of a red check before the
//! merge asks again the units the red failed, as a post run on main does (G540), and so does its
//! base run: a change that still fails them merges as no worse than main, and main stays red; a
//! change whose run passes them clears the red.
//!
//! Guards to mutate: in `src/publish.rs` `red_gate`, no units for a red (the first case clears the
//! red); in `src/check.rs` `base_answer`, a base run asked no units (the first case is refused for
//! main's own failure).

// Each case runs a shell script (the check's adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// Main red on `suite`. While `src/bad.rs` exists, a run asked about that file, or asked to
/// run `t1` again, fails `t1`; any other run selects `t2` alone, and passes. Main's tip then
/// lands a file outside the check's paths, so a run on main's own diff selects no failing unit.
fn red_main() -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let other = passing_result(1).replace("t1", "t2");
    let mut files = check_files(
        &format!(
            "[ \"$1\" = suite ] && [ -e src/bad.rs ] && case \"$req\" in \
             *'\"rerun\"'*|*src/bad.rs*) {{ {failing}; exit 0; }} ;; \
             *) {{ {other}; exit 0; }} ;; esac"
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
    let view = edited(&test_box, &repo, "BAD", "src/", "src/bad.rs");
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(landed["decision"], "merged", "{code} {landed}");
    let out = test_box
        .bay(&repo)
        .args(["--json", "runner", "run", "--once"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let view = edited(&test_box, &repo, "DOCS", "docs/", "docs/a.md");
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(landed["decision"], "merged", "{code} {landed}");
    assert_eq!(red_of(&test_box, &repo), "suite");
    (test_box, repo)
}

fn edited(test_box: &TestBox, repo: &Path, intent: &str, dir: &str, file: &str) -> PathBuf {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", dir]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let path = view.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "x\n").unwrap();
    view
}

/// The check main is red on, as `bay status` reads it: null when main is green.
fn red_of(test_box: &TestBox, repo: &Path) -> serde_json::Value {
    test_box.bay_ok(repo, &["status"])["detail"]["red"][0]["check"].clone()
}

#[test]
fn a_change_that_still_fails_a_reds_units_merges_and_main_stays_red() {
    let (test_box, repo) = red_main();
    let view = edited(&test_box, &repo, "OTHER", "src/", "src/c.rs");

    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    // Its run was asked `t1` again and failed it, as main's own run does: no worse than main.
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    assert_eq!(
        merged["detail"]["red_checked"],
        serde_json::json!(["suite"]),
        "{merged}"
    );
    assert_eq!(
        red_of(&test_box, &repo),
        "suite",
        "a run that never ran the red's failed unit cleared it"
    );
}

#[test]
fn a_change_whose_run_passes_a_reds_units_clears_it() {
    let (test_box, repo) = red_main();
    let started = test_box.bay_ok(&repo, &["start", "REPAIR", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::remove_file(view.join("src/bad.rs")).unwrap();

    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    assert!(
        red_of(&test_box, &repo).is_null(),
        "a run that passed the red's failed unit left main red"
    );
}

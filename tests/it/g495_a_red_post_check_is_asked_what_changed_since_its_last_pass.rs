//! G495 (a fleet finding, 2026-10-29: after its red, edge-py-fast answered "collected 0 of 1" on
//! four tips in a row, so it could never pass again and every change on its paths waited). A red
//! post check is asked what changed since its newest pass before the failure, so a suite that picks
//! its tests by changed paths runs the failing tests again.
//!
//! G492 runs a red check on each new tip. G439 asks a post run what changed since this box last
//! answered the check. For a red check that answer is the previous tip, so the range was the last
//! landing alone: a fix that landed off the check's paths woke none of the failing tests, the run
//! collected nothing (`unknown`), and the red stayed. G492's own test did not see it, because its
//! adapter runs the same test whatever changed.
//!
//! The guard to mutate: the red case of `since` in `bisect::tip_runs` (`src/bisect.rs`). Ask a red
//! check about the range since its last record, or since the red landing's parent, and the first
//! case stays red; drop the window fallback and the second does.

// Each case runs a shell adapter, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// A shell line that prints a `bay.check.result/1` with one unit per word of `$units`. The unit
/// `edge/b.py` fails while `conf/ok` is missing; every other unit passes.
const RESULT: &str = r#"checks=""; n=0; for unit in $units; do n=$((n+1)); v=pass; if [ "$unit" = edge/b.py ] && [ ! -f conf/ok ]; then v=product_failure; fi; [ -n "$checks" ] && checks="$checks,"; checks="$checks{\"id\":\"$unit\",\"verdict\":\"$v\"}"; done; printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"discovered":%d,"selected":%d,"completed":%d},"checks":[%s]}' "$commit" "$tree" "$n" "$n" "$n" "$checks""#;

/// A post check `suite` that wakes on `edge/**` and, like a suite that picks its tests by the
/// changed paths, runs one unit per changed path under `edge/` and none for any other change.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"edge/**\"]\n";

/// A trunk with the pre check `tests` and the post check `suite`; returns the box and repo.
fn trunk() -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let suite = format!(
        r#"if [ "$1" = suite ]; then units=$(printf '%s' "$req" | grep -o '"edge/[^"]*"' | tr -d '"' | sort -u); {RESULT}; exit 0; fi"#
    );
    let mut files = check_files(&suite, &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\nbisect_post = false\n".to_owned(),
    ));
    test_box.seed_with_check(
        &repo,
        &[("edge/a.py", "a = 1\n"), ("docs/readme.md", "readme\n")],
        &files,
    );
    (test_box, repo)
}

/// Lands a change that writes `path`.
fn land(test_box: &TestBox, repo: &Path, path: &str) {
    let dir = path.split('/').next().unwrap();
    let started = test_box.bay_ok(repo, &["start", path, "--paths", dir]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let file = view.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, format!("{path}\n")).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
}

/// One runner pass in `repo`, then `bay status`.
fn pass(test_box: &TestBox, repo: &Path) -> (Value, Value) {
    let ran = test_box.bay_ok(repo, &["runner", "run", "--once"]);
    (ran, test_box.bay_ok(repo, &["status"]))
}

/// Whether `suite` is red in `status`.
fn suite_red(status: &Value) -> bool {
    status["detail"]["red"]
        .as_array()
        .is_some_and(|red| red.iter().any(|red| red["check"] == "suite"))
}

#[test]
fn a_fix_off_the_paths_of_a_red_check_reruns_the_tests_that_failed() {
    let (test_box, repo) = trunk();
    land(&test_box, &repo, "edge/a2.py");
    let (ran, status) = pass(&test_box, &repo);
    assert!(
        !suite_red(&status),
        "suite passes on edge/a2.py: {ran}\n{status}"
    );
    // The failing test comes in with edge/b.py; a docs landing after it is the red's own landing.
    land(&test_box, &repo, "edge/b.py");
    land(&test_box, &repo, "docs/c.md");
    let (ran, status) = pass(&test_box, &repo);
    assert!(
        suite_red(&status),
        "suite is red on edge/b.py: {ran}\n{status}"
    );
    // The fix lands off suite's paths. The run on its tip must test edge/b.py again.
    land(&test_box, &repo, "conf/ok");
    let (ran, status) = pass(&test_box, &repo);
    assert!(
        !suite_red(&status),
        "the red check was asked about the fix's landing alone and tested nothing: {ran}\n{status}"
    );
}

#[test]
fn a_red_check_with_no_pass_in_the_window_is_asked_about_the_whole_span() {
    let (test_box, repo) = trunk();
    land(&test_box, &repo, "edge/b.py");
    let (ran, status) = pass(&test_box, &repo);
    assert!(
        suite_red(&status),
        "suite is red on edge/b.py: {ran}\n{status}"
    );
    land(&test_box, &repo, "conf/ok");
    let (ran, status) = pass(&test_box, &repo);
    assert!(
        !suite_red(&status),
        "a red check with no pass was asked about the fix's landing alone: {ran}\n{status}"
    );
}

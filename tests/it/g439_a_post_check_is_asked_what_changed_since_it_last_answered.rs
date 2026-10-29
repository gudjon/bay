//! G439. A post check on main's tip is asked what changed since it last answered.
//!
//! A runner runs each post check once on main's newest tip. It sent the check the changed paths
//! of that tip's last landing alone, and ran it whatever its `when_paths` said, so a suite that
//! picks its tests by changed paths ran nothing ("collected 0 of 1") unless that one landing
//! touched them; on 2026-10-28 two such suites answered nothing for hours. Now a post run names
//! what changed since the newest landing this box holds a record of the check on, and a tip
//! whose range wakes none of the check's paths is not run.
//!
//! The guard to mutate: the `since` range and the `policy.unwoken` skip in `bisect::tips`
//! (`src/bisect.rs`): ask about the last landing alone and the suite answers unknown on a tip
//! whose last landing did not touch it.

// Each case runs a shell adapter, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// A shell line that prints a `bay.check.result/1` whose passing units are the words of `$units`.
const RESULT: &str = r#"checks=""; n=0; for unit in $units; do n=$((n+1)); [ -n "$checks" ] && checks="$checks,"; checks="$checks{\"id\":\"$unit\",\"verdict\":\"pass\"}"; done; printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"discovered":%d,"selected":%d,"completed":%d},"checks":[%s]}' "$commit" "$tree" "$n" "$n" "$n" "$checks""#;

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
        "[runner]\npost = true\n".to_owned(),
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
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
}

/// One runner pass in `repo`.
fn pass(test_box: &TestBox, repo: &Path) -> Value {
    test_box.bay_ok(repo, &["runner", "run", "--once"])
}

/// This box's records of `suite` on main's tip tree.
fn suite_records(test_box: &TestBox) -> Vec<Value> {
    let remote = test_box.path().join("repo-remote.git");
    let tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    let notes = test_box
        .git(&remote)
        .args([
            "--git-dir",
            remote.to_str().unwrap(),
            "notes",
            "--ref=refs/notes/bay/evidence/local",
            "show",
            tree.trim(),
        ])
        .output()
        .unwrap();
    if !notes.status.success() {
        return Vec::new();
    }
    serde_json::from_slice::<Vec<Value>>(&notes.stdout)
        .unwrap_or_default()
        .into_iter()
        .filter(|record| record["check"]["id"] == "suite")
        .collect()
}

#[test]
fn a_post_run_names_every_path_changed_since_its_last_answer() {
    let (test_box, repo) = trunk();
    land(&test_box, &repo, "edge/a.py");
    let _ = pass(&test_box, &repo);
    assert_eq!(
        suite_records(&test_box).len(),
        1,
        "suite ran on the first tip"
    );
    // Two landings before the next pass: the edge one first, a docs one last.
    land(&test_box, &repo, "edge/b.py");
    land(&test_box, &repo, "docs/c.md");
    let ran = pass(&test_box, &repo);
    let records = suite_records(&test_box);
    let record = records
        .first()
        .unwrap_or_else(|| panic!("suite did not run on the tip: {ran}"));
    assert_eq!(
        record["verdict"], "pass",
        "suite was asked about the last landing alone: {record}"
    );
    let units: Vec<&str> = record["units"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|unit| unit["id"].as_str())
        .collect();
    assert!(units.contains(&"edge/b.py"), "{record}");
}

#[test]
fn a_tip_whose_changes_wake_nothing_of_a_post_check_does_not_run_it() {
    let (test_box, repo) = trunk();
    land(&test_box, &repo, "edge/a.py");
    let _ = pass(&test_box, &repo);
    land(&test_box, &repo, "docs/d.md");
    let ran = pass(&test_box, &repo);
    assert!(
        suite_records(&test_box).is_empty(),
        "suite ran on a tip whose changes since its last answer are docs only: {ran}"
    );
}

//! G585 (a fleet finding, 2026-10-30: a red main's repair, placed behind twenty changes, waited
//! 14 minutes, since each pass ran out its bound before it reached the repair's place). A pass
//! takes a change that names one it fixes, reverts or supersedes before every other change, so
//! the repair of a red main runs in the first pass that sees it.
//!
//! Guard to mutate, in `src/executor.rs`: `in_pass_order` without its repair clause (a repair
//! keeps its turn after the changes submitted before it).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// The changes whose operations a pass's `detail.ran` names, in order.
fn ran(pass: &Value) -> Vec<String> {
    pass["detail"]["ran"]
        .as_array()
        .map_or_else(Vec::new, |rows| {
            rows.iter()
                .filter(|row| row["op_id"].is_string())
                .filter_map(|row| row["change"].as_str().map(str::to_owned))
                .collect()
        })
}

/// Starts a change that writes `path`, with `--fixes fixes` when it names one, and submits it
/// with `merge_args`: its change id and the result.
fn submit(
    test_box: &TestBox,
    repo: &Path,
    path: &str,
    fixes: Option<&str>,
    merge_args: &[&str],
) -> (String, Value) {
    let mut args = vec!["start", path, "--paths", path];
    if let Some(fixes) = fixes {
        args.extend(["--fixes", fixes]);
    }
    let started = test_box.bay_ok(repo, &args);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let file = view.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, format!("// {path}\n")).unwrap();
    let (code, result) = result_json(test_box.bay(&view).args(merge_args));
    assert_eq!(code, 0, "{result}");
    (id, result)
}

#[test]
fn a_repair_submitted_last_runs_first_in_a_pass() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = check_files("true", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    // One runner writes every record here, so no two producers share a name.
    let executor = test_box.clone_of(&remote, "executor");
    let pass = |test_box: &TestBox, bound: &str| {
        let (code, ran) = result_json(
            test_box
                .bay(&executor)
                .env("BAY_TEST_PASS_BOUND_S", bound)
                .args(["runner", "run", "--once"]),
        );
        assert_eq!(code, 0, "{ran}");
        ran
    };
    let landed = submit(&test_box, &repo, "src/b.rs", None, &["merge"]).0;
    let landing = pass(&test_box, "60");
    assert_eq!(ran(&landing), vec![landed.clone()], "{landing}");
    let first = submit(&test_box, &repo, "src/c.rs", None, &["merge"]).0;
    submit(&test_box, &repo, "src/d.rs", None, &["merge"]);
    let repair = submit(&test_box, &repo, "src/e.rs", Some(&landed), &["merge"]).0;

    let one = pass(&test_box, "0");
    assert_eq!(
        ran(&one),
        vec![repair.clone()],
        "the repair runs first: {one}"
    );
    let two = pass(&test_box, "0");
    assert_eq!(ran(&two), vec![first], "then the line, in order: {two}");
}

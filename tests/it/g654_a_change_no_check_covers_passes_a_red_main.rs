//! G654 (owner ruling, 2026-11-05; a fleet finding: a document waited two hours behind a red check
//! it could not worsen). A repository declares in `[policy] passes_red` the paths whose changes may
//! merge past a red main. A change passes when every file is on that list and no pre-merge check
//! covers it, and its result names each red check it passed in `detail.passed_red`. The list starts
//! empty; every other change on a red check's paths runs that check before its merge (G677,
//! `detail.red_checked`). A list, because "no pre-merge check covers it" is true of code a
//! repository checks only after the merge, which must still run its own red check (G577).
//!
//! Guards to mutate, in `src/publish.rs` `red_gate`: the `may_pass_red` test (the unlisted document
//! and the mixed change run no red check); the `needs_no_check` test (the listed file a pre-merge
//! check covers runs none); the `passed_red` it records (the name is missing).

// Each case runs a shell script (the check's adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n";

/// What may pass a red main: a document, and a file a pre-merge check covers, to show the list
/// alone is not enough.
const LIST: &str = "\n[policy]\npasses_red = [\"docs/**\", \"src/listed.rs\"]\n";

/// A main whose post check `suite`, which reads every path, fails on any tree holding
/// `src/bad.rs`, and whose pre-merge check reads only `src/`. The bad file is merged, so main is
/// red.
fn red_main() -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let mut files = check_files(
        &format!("[ \"$1\" = suite ] && [ -e src/bad.rs ] && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            *text = text.replacen("timeout_s", "when_paths = [\"src/**\"]\ntimeout_s", 1);
            text.push_str(POST);
            text.push_str(LIST);
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\n".to_owned(),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
        &files,
    );
    let (code, landed) = merge(&test_box, &repo, "BAD", "src/", "src/bad.rs");
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let out = test_box
        .bay(&repo)
        .args(["--json", "runner", "run", "--once"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let status = test_box.bay_ok(&repo, &["status"]);
    assert_eq!(status["detail"]["red"][0]["check"], "suite", "{status}");
    (test_box, repo)
}

fn merge(test_box: &TestBox, repo: &Path, intent: &str, paths: &str, file: &str) -> (i32, Value) {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", paths]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::create_dir_all(view.join(file).parent().unwrap()).unwrap();
    std::fs::write(view.join(file), "new\n").unwrap();
    result_json(test_box.bay(&view).args(["merge", "--wait"]))
}

#[test]
fn a_document_merges_past_a_red_main_and_names_what_it_passed() {
    let (test_box, repo) = red_main();

    let (code, landed) = merge(&test_box, &repo, "NOTE", "docs/", "docs/note.md");

    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(
        landed["detail"]["passed_red"],
        serde_json::json!(["suite"]),
        "{landed}"
    );
}

#[test]
fn a_change_a_pre_merge_check_covers_runs_the_red_check() {
    let (test_box, repo) = red_main();

    let (code, checked) = merge(&test_box, &repo, "CODE", "src/", "src/c.rs");

    assert_eq!(code, 0, "{checked}");
    assert_eq!(checked["decision"], "merged", "{checked}");
    assert_eq!(
        checked["detail"]["red_checked"],
        serde_json::json!(["suite"]),
        "{checked}"
    );
}

#[test]
fn a_document_off_the_list_runs_the_red_check() {
    let (test_box, repo) = red_main();
    let (code, checked) = merge(&test_box, &repo, "OTHER", "notes/", "notes/note.md");
    assert_eq!(code, 0, "{checked}");
    assert_eq!(
        checked["detail"]["red_checked"],
        serde_json::json!(["suite"]),
        "{checked}"
    );
}

#[test]
fn a_change_with_one_file_off_the_list_runs_the_red_check() {
    let (test_box, repo) = red_main();
    let started = test_box.bay_ok(&repo, &["start", "MIXED", "--paths", "docs/", "notes/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("docs/note.md"), "new\n").unwrap();
    std::fs::create_dir_all(view.join("notes")).unwrap();
    std::fs::write(view.join("notes/note.md"), "new\n").unwrap();
    let (code, checked) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{checked}");
    assert_eq!(
        checked["detail"]["red_checked"],
        serde_json::json!(["suite"]),
        "{checked}"
    );
}

#[test]
fn a_listed_file_a_pre_merge_check_covers_runs_the_red_check() {
    let (test_box, repo) = red_main();
    let (code, checked) = merge(&test_box, &repo, "LISTED", "src/", "src/listed.rs");
    assert_eq!(code, 0, "{checked}");
    assert_eq!(
        checked["detail"]["red_checked"],
        serde_json::json!(["suite"]),
        "{checked}"
    );
}

#[test]
fn a_green_main_names_nothing_passed() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            *text = text.replacen("timeout_s", "when_paths = [\"src/**\"]\ntimeout_s", 1);
        }
    }
    test_box.seed_with_check(&repo, &[("docs/a.md", "a\n")], &files);

    let (code, landed) = merge(&test_box, &repo, "NOTE", "docs/", "docs/note.md");

    assert_eq!(code, 0, "{landed}");
    assert!(landed["detail"].get("passed_red").is_none(), "{landed}");
}

//! G675 (a lint change sat blocked 262 s with no reason, found only by polling: it edited a check's
//! script and had not raised the policy's `revision`). `revision` only says the policy changed, and
//! bay knows the number, so a change that edits the policy or a check's own files merges with the
//! revision raised by bay, above main's. A change that raised it itself keeps its number.
//!
//! Guard to mutate, in `src/merge.rs` `raise_revision`: return `tree` at once (the edit is refused
//! "must raise revision").

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn an_edit_of_a_checks_script_merges_with_the_next_revision() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("", &passing_result(2), 2, 60),
    );
    let started = test_box.bay_ok(&repo, &["start", "check edit", "--paths", "scripts/"]);
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let script = view.join("scripts/bay-check");
    let text = std::fs::read_to_string(&script).unwrap();
    std::fs::write(&script, format!("{text}\n# a comment\n")).unwrap();

    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    let remote = test_box.path().join("repo-remote.git");
    let policy = test_box.git_bare(&remote, &["show", "main:.bay/policy.toml"]);
    assert!(policy.contains("revision = 2"), "{policy}");
}

//! G412 (fleet findings, 2026-10-27: `bay start <change> --after <id>` and
//! `bay start <change> --fixes <id>`, run to refresh a change in place, opened a new empty change
//! whose intent was the first change's id; `--paths a,b,c` was read as one path named `a,b,c`).
//! A start that names an existing change with what only a new change takes is refused and says
//! how to refresh in place; a comma-joined path main does not hold is refused as one. Nothing is
//! created by either.
//!
//! Guards to mutate: `src/start.rs` `named_or_refused`, its first two checks.

use crate::support;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn naming_a_change_with_after_is_refused_and_says_how_to_refresh() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let (id, _) = test_box.start_and_edit(&repo);
    let before = test_box.git_ok(
        &repo,
        &["for-each-ref", "--format=%(refname)", "refs/bay/changes/"],
    );
    for flag in ["--after", "--fixes"] {
        let (code, refused) = result_json(
            test_box
                .bay(&repo)
                .args(["start", &id, flag, &id, "--paths", "src/"]),
        );
        assert_ne!(code, 0, "{refused}");
        let text = refused.to_string();
        assert!(text.contains(&format!("bay start {id}")), "{refused}");
    }
    let after = test_box.git_ok(
        &repo,
        &["for-each-ref", "--format=%(refname)", "refs/bay/changes/"],
    );
    assert_eq!(before, after, "a start that was refused made a change");
}

#[test]
fn comma_joined_paths_are_refused_as_one_path() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")],
        &unchecked_files(),
    );
    let (code, refused) = result_json(test_box.bay(&repo).args([
        "start",
        "two files",
        "--paths",
        "src/a.rs,src/b.rs",
    ]));
    assert_ne!(code, 0, "{refused}");
    assert!(
        refused.to_string().contains("separated by spaces"),
        "{refused}"
    );
}

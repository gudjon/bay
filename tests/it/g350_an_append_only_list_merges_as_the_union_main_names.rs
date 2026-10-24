//! G350 (owner ruling 2026-10-24, BACKLOG 67 item 1a): an append-only list merges as the union
//! main names.
//!
//! Card lists, registries and baselines are lists each change appends to, so two open changes
//! that each add a line conflicted, and the second had to be redone by hand. Git already says
//! how such a file merges: `merge=union` in `.gitattributes`. bay's candidate merge reads the
//! attributes from main, never from the change merged onto it, so both lines merge, and a change
//! cannot pick a union merge for a file main does not name.
//!
//! Guard to mutate: `src/merge.rs` `merge_tree`: drop the `ATTR_SOURCE` variable (the second
//! append is refused `MERGE_CONFLICT`).

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, UNCHECKED_POLICY, result_json};

/// A repository whose main names `list.txt` an append-only list, with no check.
fn seeded(test_box: &TestBox) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[
            ("list.txt", "one\n"),
            ("src/a.rs", "fn a() {}\n"),
            (".gitattributes", "list.txt merge=union\n"),
        ],
        &[(".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned())],
    );
    (repo, test_box.path().join("repo-remote.git"))
}

/// A change opened in `repo` on `paths`, with `file` written as `text`; returns its view.
fn opened(test_box: &TestBox, repo: &Path, intent: &str, file: &str, text: &str) -> PathBuf {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "."]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), text).unwrap();
    view
}

#[test]
fn two_appends_to_a_list_main_names_both_merge() {
    let test_box = TestBox::new();
    let (repo, remote) = seeded(&test_box);
    let first = opened(&test_box, &repo, "add two", "list.txt", "one\ntwo\n");
    let second = opened(&test_box, &repo, "add three", "list.txt", "one\nthree\n");
    for view in [&first, &second] {
        let (code, merged) = result_json(test_box.bay(view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{merged}");
        assert_eq!(merged["decision"], "merged", "{merged}");
    }
    let list = test_box.git_bare(&remote, &["show", "main:list.txt"]);
    assert!(
        list.contains("two") && list.contains("three"),
        "both appends: {list}"
    );
}

#[test]
fn a_change_cannot_pick_a_union_merge_for_itself() {
    let test_box = TestBox::new();
    let (repo, _remote) = seeded(&test_box);
    let first = opened(&test_box, &repo, "edit a", "src/a.rs", "fn a1() {}\n");
    // The second change edits the same line and names src/a.rs a union file in its own tree.
    let second = opened(&test_box, &repo, "edit a too", "src/a.rs", "fn a2() {}\n");
    std::fs::write(
        second.join(".gitattributes"),
        "list.txt merge=union\nsrc/a.rs merge=union\n",
    )
    .unwrap();
    let (code, merged) = result_json(test_box.bay(&first).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    let (code, refused) = result_json(test_box.bay(&second).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "MERGE_CONFLICT", "{refused}");
}

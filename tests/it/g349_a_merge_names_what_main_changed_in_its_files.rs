//! G349 (owner ruling 2026-10-24, BACKLOG 67 item 9): a merge names what main changed in its
//! files since its base.
//!
//! A fleet's groom moved files under changes still open, and their agents learned it only when
//! a check failed. The candidate is still checked on the merged result; `bay merge` now says
//! first, in `detail.main_changed`, which of the change's own files main changed since the
//! change's base, and main's newest commit that changed them. A change whose files main left
//! alone is told nothing.
//!
//! Guard to mutate: `src/publish.rs` `main_changed`: return `None` at once (the merge says
//! nothing of main's change).

use crate::support;

use std::path::PathBuf;

use serde_json::json;
use support::{TestBox, UNCHECKED_POLICY, result_json};

#[test]
fn a_merge_names_the_files_main_changed_and_the_commit() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n\n\n\nfn a2() {}\n"),
            ("src/b.rs", "fn b() {}\n"),
        ],
        &[(".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned())],
    );
    let remote = test_box.path().join("repo-remote.git");
    let other = test_box.clone_of(&remote, "other");

    // Two changes open on the same main: one edits a.rs, the other only b.rs.
    let started = test_box.bay_ok(&repo, &["start", "edit a", "--paths", "src/"]);
    let edits_a = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(edits_a.join("src/a.rs"), "fn a() {}\n\n\n\nfn a3() {}\n").unwrap();
    let started = test_box.bay_ok(&repo, &["start", "edit b", "--paths", "src/"]);
    let edits_b = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(edits_b.join("src/b.rs"), "fn b2() {}\n").unwrap();

    // Main changes a.rs at its other end, from another clone.
    std::fs::write(other.join("src/a.rs"), "fn a1() {}\n\n\n\nfn a2() {}\n").unwrap();
    let (code, saved) = result_json(test_box.bay(&other).args(["save", "rename a"]));
    assert_eq!(code, 0, "{saved}");
    let main = test_box.git_bare(&remote, &["rev-parse", "main"]);

    let (code, merged) = result_json(test_box.bay(&edits_a).args(["merge"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(
        merged["detail"]["main_changed"],
        json!({"paths": ["src/a.rs"], "commit": main}),
        "{merged}"
    );

    let (code, merged) = result_json(test_box.bay(&edits_b).args(["merge"]));
    assert_eq!(code, 0, "{merged}");
    assert!(
        merged["detail"].get("main_changed").is_none(),
        "main left b.rs alone: {merged}"
    );
}

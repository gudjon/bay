//! G812 (a measurement, 2026-11-20: every one of a runner's 48,578 kept files, 3 GB, had been
//! written within minutes: the hourly prune removed each file no tree linked between two checks,
//! and the next check wrote the whole tree again). A kept file no tree links stays until no tree
//! has linked it for a day, so the next check links it instead of writing it.
//!
//! Guard to mutate: in `src/materialize.rs` `Store::prune`, drop the `unused_since` condition
//! (the file a tree just let go is removed).

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result};

#[test]
fn a_kept_file_a_tree_just_let_go_stays_for_the_next_check() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    // A kept file whose tree was just removed: no tree links it now.
    let shard = repo.join(".git/bay/blobs/ab");
    std::fs::create_dir_all(&shard).unwrap();
    let kept = shard.join("ab00000000000000000000000000000000000003");
    std::fs::write(&kept, b"a file of the last check's tree").unwrap();
    let tree = test_box.path().join("last-tree");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::hard_link(&kept, tree.join("file")).unwrap();
    std::fs::remove_dir_all(&tree).unwrap();

    // WHEN a start runs the prune
    let started = test_box.bay_ok(&repo, &["start", "another", "--paths", "src", "--new"]);

    // THEN the file stays for the next check to link
    assert!(
        kept.exists(),
        "a kept file a tree let go a moment ago was pruned: {started}"
    );
}

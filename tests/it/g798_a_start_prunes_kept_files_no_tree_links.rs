//! G798 (a measurement, 2026-11-19: a Mac's store kept 2.9 GB of files that no tree linked, a
//! whole check tree's worth, because the kept files are pruned only when a check makes a tree,
//! and none had run there since). A start prunes them on the same hourly rule: a kept file no
//! tree links goes, and one a tree still links stays.
//!
//! Guard to mutate: in `src/start.rs` `reclaim`, drop the `prune_kept` call (the unlinked file
//! stays).

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result};

#[test]
fn a_start_prunes_the_kept_files_no_tree_links() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    // Two kept files: one no tree links, and one a tree elsewhere still links.
    let shard = repo.join(".git/bay/blobs/ab");
    std::fs::create_dir_all(&shard).unwrap();
    let unlinked = shard.join("ab00000000000000000000000000000000000001");
    let linked = shard.join("ab00000000000000000000000000000000000002");
    std::fs::write(&unlinked, b"old tree file").unwrap();
    std::fs::write(&linked, b"live tree file").unwrap();
    let tree = test_box.path().join("a-tree");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::hard_link(&linked, tree.join("file")).unwrap();

    // WHEN a change starts
    let started = test_box.bay_ok(&repo, &["start", "another", "--paths", "src", "--new"]);

    // THEN the unlinked file is gone and the linked one stays
    assert!(
        !unlinked.exists(),
        "the unlinked kept file stayed: {started}"
    );
    assert!(linked.exists(), "a kept file a tree links went: {started}");
}

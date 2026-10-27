//! G419 (a fleet finding, 2026-10-27: a copy of a change chain under a name that is no change id,
//! `refs/bay/changes/hvvrfutj-r`, made outside bay, was deleted on the remote and pushed back
//! within hours by a store that still held it). bay's chain sync pushes only refs whose name is
//! a change id; a foreign copy stays where it is.
//!
//! Guard to mutate: `src/receipts.rs` `sync_chains`: drop the `is_change_id` check.

use crate::support;

use support::{TestBox, unchecked_files};

#[test]
fn a_foreign_copy_of_a_chain_is_never_pushed() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let (id, _) = test_box.start_and_edit(&repo);
    let chain = format!("refs/bay/changes/{id}");
    let head = test_box.git_ok(&repo, &["rev-parse", &chain]);
    let foreign = format!("refs/bay/changes/{id}-r");
    test_box.git_ok(&repo, &["update-ref", &foreign, head.trim()]);

    test_box.bay_ok(&repo, &["status"]);
    let listed = test_box.git_bare(
        &remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/changes/"],
    );
    assert!(
        listed.contains(&chain),
        "the change's own chain is on the remote: {listed}"
    );
    assert!(
        !listed.contains(&foreign),
        "a foreign copy was pushed: {listed}"
    );
}

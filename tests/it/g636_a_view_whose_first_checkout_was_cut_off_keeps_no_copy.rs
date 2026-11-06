//! G636 (a fleet finding, 2026-11-02): a full view's first checkout was cut off before git wrote
//! its index, so git read every file of the view as untracked. `bay start <change>`, the repair
//! for a half-made view, then moved the whole view aside as unsaved work (44,795 files), and
//! made it again. A view with no index is judged against its commit: a file it holds as that
//! commit has it is no work, and a file that differs, or that the commit lacks, still is.
//!
//! Guard to mutate: `src/view.rs` `holds_unsaved`: judge with the view's own index whether it
//! has one or not (the first case keeps a copy aside).

use crate::support;

use std::path::{Path, PathBuf};

use support::TestBox;

/// A started change on `src/` whose view is left as a cut-off first checkout: its journal
/// gone, its index gone and, when `locked`, an `index.lock` left beside it. The change's id
/// and view.
fn cut_off(test_box: &TestBox, repo: &Path, locked: bool) -> (String, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", "CARD-1", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let index = test_box.git_ok(&view, &["rev-parse", "--git-path", "index"]);
    let index = view.join(index);
    assert!(index.exists(), "{}", index.display());
    std::fs::remove_file(&index).unwrap();
    if locked {
        std::fs::write(index.with_extension("lock"), "").unwrap();
    }
    let journal = test_box.views_dir(repo).join(format!("{id}.journal"));
    std::fs::remove_file(&journal).unwrap();
    (id, view)
}

/// The repository, seeded with two files under `src/`.
fn seeded(test_box: &TestBox) -> PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")],
    );
    repo
}

/// Each `.unsaved-*` copy under the views folder of `repo`.
fn copies(test_box: &TestBox, repo: &Path) -> Vec<String> {
    std::fs::read_dir(test_box.views_dir(repo))
        .unwrap()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.contains(".unsaved-"))
        .collect()
}

#[test]
fn a_view_with_no_index_and_no_edits_is_made_again_with_nothing_kept() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let (id, view) = cut_off(&test_box, &repo, false);

    let resumed = test_box.bay_ok(&repo, &["start", &id]);
    assert!(resumed["detail"]["unsaved_kept"].is_null(), "{resumed}");
    assert!(
        copies(&test_box, &repo).is_empty(),
        "{:?}",
        copies(&test_box, &repo)
    );
    // THEN the view is whole again: its files as the draft has them, and an index git reads.
    assert_eq!(
        std::fs::read_to_string(view.join("src/a.rs")).unwrap(),
        "fn a() {}\n"
    );
    let status = test_box.git_ok(&view, &["status", "--porcelain"]);
    assert!(status.is_empty(), "{status}");
}

#[test]
fn a_view_with_no_index_keeps_its_edits_and_new_files() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let (id, view) = cut_off(&test_box, &repo, false);
    std::fs::write(view.join("src/a.rs"), "fn a() { edited }\n").unwrap();
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();

    let resumed = test_box.bay_ok(&repo, &["start", &id]);
    let kept = PathBuf::from(
        resumed["detail"]["unsaved_kept"]
            .as_str()
            .unwrap_or_else(|| panic!("no unsaved work was kept: {resumed}")),
    );
    assert_eq!(
        std::fs::read_to_string(kept.join("src/a.rs")).unwrap(),
        "fn a() { edited }\n"
    );
    assert_eq!(
        std::fs::read_to_string(kept.join("src/new.rs")).unwrap(),
        "fn new() {}\n"
    );
}

#[test]
fn a_view_with_no_index_and_a_lock_left_is_made_again_with_nothing_kept() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let (id, view) = cut_off(&test_box, &repo, true);

    let resumed = test_box.bay_ok(&repo, &["start", &id]);
    assert!(resumed["detail"]["unsaved_kept"].is_null(), "{resumed}");
    assert!(
        copies(&test_box, &repo).is_empty(),
        "{:?}",
        copies(&test_box, &repo)
    );
    let status = test_box.git_ok(&view, &["status", "--porcelain"]);
    assert!(status.is_empty(), "{status}");
}

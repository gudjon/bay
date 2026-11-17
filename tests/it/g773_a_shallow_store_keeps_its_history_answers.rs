//! G773 (a measurement, 2026-11-17: on a busy box whose store is a shallow clone, every `bay
//! status` walked the same history again, a `rev-list` or `log` of fixed commits 300 to 500 ms
//! each under load, 4 s a listing; the panel that reads it sat on "Reading bay…"). A shallow
//! store keeps the answers of history queries of fixed commits across processes, as a full store
//! does (G721), under its boundary: once the boundary moves, they are asked of Git again. Read
//! from the trace a test build writes (G171).
//!
//! Guard to mutate: in `src/git.rs` `fact_context`, return `None` for a store with a `shallow`
//! file again, and the second read walks the history again.

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::{TestBox, unchecked_files};

/// The history queries (`log`, `rev-list`, `merge-base`) one `bay status` in `repo` ran.
fn history_walks(test_box: &TestBox, repo: &Path, name: &str) -> usize {
    let trace = test_box.path().join(format!("{name}.jsonl"));
    let _ = std::fs::remove_file(&trace);
    let (code, status) = support::result_json(
        test_box
            .bay(repo)
            .args(["status"])
            .env("BAY_TEST_SAVED", "off")
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{status}");
    let text = std::fs::read_to_string(&trace).unwrap_or_default();
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| {
            matches!(
                line["sub"].as_str(),
                Some("log" | "rev-list" | "merge-base")
            )
        })
        .count()
}

/// A seeded repository with three changes, and its store's common directory.
fn seeded(test_box: &TestBox) -> (std::path::PathBuf, std::path::PathBuf) {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    for n in 0..3 {
        test_box.bay_ok(&repo, &["start", &format!("CARD-{n}"), "--paths", "src/"]);
    }
    let common = test_box.git_ok(
        &repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    (repo, std::path::PathBuf::from(common.trim()))
}

/// Marks the store at `common` shallow at `boundary`, a commit with no parent, so its history
/// reads the same.
fn shallow_at(common: &Path, boundary: &str) {
    std::fs::write(common.join("shallow"), format!("{boundary}\n")).unwrap();
}

#[test]
fn a_shallow_store_answers_a_read_again_as_a_full_store_does() {
    // GIVEN a full store and a shallow one, each read once
    let full_box = TestBox::new();
    let (full, _) = seeded(&full_box);
    let _ = history_walks(&full_box, &full, "full-first");
    let shallow_box = TestBox::new();
    let (shallow, common) = seeded(&shallow_box);
    let root = shallow_box.git_ok(&shallow, &["rev-list", "--max-parents=0", "HEAD"]);
    shallow_at(&common, root.lines().next().unwrap().trim());
    let first = history_walks(&shallow_box, &shallow, "shallow-first");

    // WHEN each is read again
    let full_again = history_walks(&full_box, &full, "full-again");
    let shallow_again = history_walks(&shallow_box, &shallow, "shallow-again");

    // THEN the shallow store walks no more history than the full one: its answers were kept
    assert!(
        full_again < first,
        "the full store kept nothing: {full_again} of {first}"
    );
    assert_eq!(
        shallow_again, full_again,
        "the shallow store walked {shallow_again} times on its second read, the full one {full_again}"
    );
}

#[test]
fn a_moved_boundary_asks_git_again() {
    let test_box = TestBox::new();
    let (repo, common) = seeded(&test_box);
    let root = test_box.git_ok(&repo, &["rev-list", "--max-parents=0", "HEAD"]);
    let root = root.lines().next().unwrap().trim().to_owned();
    shallow_at(&common, &root);
    let first = history_walks(&test_box, &repo, "first");
    let kept = history_walks(&test_box, &repo, "kept");
    assert!(kept < first, "nothing was kept: {kept} of {first}");

    // WHEN the boundary moves (here: the same commit, written as another boundary file)
    std::fs::write(common.join("shallow"), format!("{root}\n{root}\n")).unwrap();

    // THEN the kept answers no longer answer: the read walks as a first one does
    let moved = history_walks(&test_box, &repo, "moved");
    assert_eq!(
        moved, first,
        "a moved boundary still read kept answers: {moved} of {first}"
    );
}

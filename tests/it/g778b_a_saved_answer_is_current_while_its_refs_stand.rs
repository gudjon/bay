//! G778b (from a terminal's open-source repository: it reads Git again only when a ref file it
//! read changed). A status answer saved for `--deadline 0` keeps a quick stamp of the store's
//! ref files and folders and of the views it names: a read that finds them as they were serves it
//! as current, with no read again behind it; a read after a ref moved serves it with its age and
//! has it read again, as G778 does.
//!
//! Guard to mutate: in `src/saved.rs` `from_store`, make `current` false, and the unmoved store's
//! answer is read again in the background.

use crate::support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

const ACTOR: &str = "seat-one";

/// A store with one change of the seat's: the repository, the change and its view.
fn store(test_box: &TestBox) -> (PathBuf, String, PathBuf) {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    (
        repo,
        started["change"].as_str().unwrap().to_owned(),
        PathBuf::from(started["detail"]["view"].as_str().unwrap()),
    )
}

/// A `--deadline 0` read of `id` in `view`, with any saved answer old enough to read again.
fn read_now(test_box: &TestBox, view: &Path, id: &str) -> Value {
    let (code, answer) = result_json(
        test_box
            .bay(view)
            .args(["status", id, "--deadline", "0"])
            .env("BAY_ACTOR", ACTOR)
            .env("BAY_TEST_REFRESH_AFTER_S", "0"),
    );
    assert_eq!(code, 0, "{answer}");
    answer
}

/// Waits until no sweep and no read again of `repo`'s store runs: each may move refs, or save.
fn settled(repo: &Path) {
    support::sweeps_ended(repo);
    let until = std::time::Instant::now() + Duration::from_secs(120);
    while std::time::Instant::now() < until && saved(repo).1 {
        std::thread::sleep(Duration::from_millis(100));
    }
    // The read again may have swept the store in its turn.
    support::sweeps_ended(repo);
}

/// The times the saved answers of `repo`'s store were saved at, and whether a read again holds
/// its lock now.
fn saved(repo: &Path) -> (Vec<u64>, bool) {
    let dir = repo.join(".git").join("bay").join("status-answers");
    let mut times = Vec::new();
    let mut refreshing = false;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("refresh") => refreshing = true,
            Some("json") => times.extend(
                std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .and_then(|saved| saved["saved"].as_u64()),
            ),
            _ => {}
        }
    }
    times.sort_unstable();
    (times, refreshing)
}

#[test]
fn an_answer_whose_refs_stand_is_current_and_one_after_a_move_is_read_again() {
    let test_box = TestBox::new();
    let (repo, id, view) = store(&test_box);
    // GIVEN the answer saved once the store settled: the first read's own fetch moves refs, and
    // the reads after it are read again until one is saved from a store that stands
    let until = std::time::Instant::now() + Duration::from_secs(120);
    settled(&repo);
    let mut again = read_now(&test_box, &view, &id);
    while again["detail"]["current"] != true && std::time::Instant::now() < until {
        settled(&repo);
        again = read_now(&test_box, &view, &id);
    }
    let before = saved(&repo).0;

    // WHEN the change is read again with nothing moved

    // THEN the saved answer is current, and nothing is read again behind it
    assert_eq!(again["detail"]["from_store"], true, "{again}");
    assert_eq!(again["detail"]["current"], true, "{again}");
    std::thread::sleep(Duration::from_secs(3));
    let (after, refreshing) = saved(&repo);
    assert!(!refreshing, "a read again started for a current answer");
    assert_eq!(after, before, "a current answer was read again");

    // WHEN a ref of the store moves, and the change is read again
    test_box.git_ok(&repo, &["update-ref", "refs/bay/moved-after-save", "HEAD"]);
    std::thread::sleep(Duration::from_millis(1100));
    let moved = read_now(&test_box, &view, &id);

    // THEN it is served with its age, not as current, and read again behind it
    assert_eq!(moved["detail"]["from_store"], true, "{moved}");
    assert_eq!(moved["detail"]["current"], false, "{moved}");
    let until = std::time::Instant::now() + Duration::from_secs(60);
    while saved(&repo).0 == before && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(250));
    }
    assert_ne!(saved(&repo).0, before, "no read again after a ref moved");
}

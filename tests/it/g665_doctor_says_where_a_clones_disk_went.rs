//! G665 (BACKLOG 75): finding where a clone's disk went took hand-run `du` and `git`. `bay doctor`
//! has one storage row: Git's objects, half-written packs, this repository's views and check
//! scratch, the worktrees bay did not make, and what the next `bay start` would free. It is read
//! only, and sizing that runs out of its time budget says `partial`.
//!
//! Guards to mutate: `src/storage.rs` `other_worktrees`, the views left out (the view is listed as
//! another's); `size_of`, the deadline (the budget case); `src/view.rs` `would_release`, `standing`
//! (a view with newer work is listed as freed); `src/doctor.rs`, the row.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

fn seeded() -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    (test_box, repo)
}

fn start(test_box: &TestBox, repo: &Path, intent: &str) -> (String, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    (
        started["change"].as_str().unwrap().to_owned(),
        PathBuf::from(started["detail"]["view"].as_str().unwrap()),
    )
}

fn storage(test_box: &TestBox, repo: &Path, envs: &[(&str, &str)]) -> Value {
    let mut command = test_box.bay(repo);
    command.envs(envs.iter().copied());
    let (_, doctor) = result_json(command.arg("doctor"));
    doctor["detail"]["rows"]
        .as_array()
        .expect("doctor rows")
        .iter()
        .find(|row| row["name"] == "storage")
        .cloned()
        .expect("a storage row")
}

/// Merges the work in `view`, which ends its change.
fn merged(test_box: &TestBox, view: &Path) {
    std::fs::write(view.join("src/a.rs"), "fn a() { 1; }\n").unwrap();
    let (code, merged) = result_json(test_box.bay(view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
}

#[test]
fn it_sizes_the_views_and_names_the_worktrees_bay_did_not_make() {
    let (test_box, repo) = seeded();
    let (id, view) = start(&test_box, &repo, "BIG");
    std::fs::write(view.join("src/big.bin"), vec![0_u8; 1 << 20]).unwrap();
    let other = test_box.path().join("mine");
    test_box.git_ok(
        &repo,
        &["worktree", "add", "-q", "--detach", other.to_str().unwrap()],
    );
    let tmp = repo.join(".git/objects/pack/tmp_pack_x");
    std::fs::create_dir_all(tmp.parent().unwrap()).unwrap();
    std::fs::write(&tmp, vec![0_u8; 2048]).unwrap();

    let row = storage(&test_box, &repo, &[]);

    assert_eq!(row["state"], "green", "{row}");
    assert_eq!(row["partial"], false, "{row}");
    assert!(row["objects_bytes"].as_u64().unwrap() > 0, "{row}");
    assert_eq!(row["views"]["count"], 1, "{row}");
    assert!(row["views"]["bytes"].as_u64().unwrap() >= 1 << 20, "{row}");
    assert_eq!(row["views"]["largest"][0]["change"], id.as_str(), "{row}");
    assert_eq!(row["half_written_packs"]["count"], 1, "{row}");
    assert_eq!(row["half_written_packs"]["bytes"], 2048, "{row}");
    assert_eq!(row["other_worktrees"]["count"], 1, "{row}");
    let named = row["other_worktrees"]["paths"][0].as_str().unwrap();
    assert!(named.ends_with("/mine"), "{row}");
    // A pack written just now is one a fetch may still be writing.
    assert_eq!(
        row["next_start_frees"]["half_written_pack_bytes"], 0,
        "{row}"
    );
}

#[test]
fn it_names_what_a_start_would_free_and_removes_nothing() {
    let (test_box, repo) = seeded();
    let (ended, view) = start(&test_box, &repo, "ENDED");
    merged(&test_box, &view);
    let (open, _) = start(&test_box, &repo, "OPEN");
    assert!(!view.exists(), "that start released the merged view");
    let (ended_too, view_too) = start(&test_box, &repo, "ENDED-TOO");
    merged(&test_box, &view_too);

    let row = storage(&test_box, &repo, &[]);

    let free = row["next_start_frees"]["views"].as_array().unwrap();
    assert_eq!(free.len(), 1, "{row}");
    assert_eq!(free[0], ended_too.as_str(), "{row}");
    assert_ne!(ended, ended_too);
    assert_ne!(free[0], open.as_str(), "{row}");
    assert!(view_too.exists(), "doctor removed a view");

    start(&test_box, &repo, "NEXT");
    assert!(!view_too.exists(), "a start did not free what doctor named");
}

#[test]
fn a_view_with_newer_work_is_not_named_as_freed() {
    let (test_box, repo) = seeded();
    let (_, view) = start(&test_box, &repo, "ENDED");
    merged(&test_box, &view);
    std::fs::write(view.join("src/after.rs"), "fn after() {}\n").unwrap();

    let row = storage(&test_box, &repo, &[]);

    assert_eq!(
        row["next_start_frees"]["views"],
        serde_json::json!([]),
        "{row}"
    );
}

#[test]
fn sizing_that_runs_out_of_time_says_so() {
    let (test_box, repo) = seeded();
    start(&test_box, &repo, "BIG");

    let row = storage(&test_box, &repo, &[("BAY_TEST_STORAGE_BUDGET_MS", "0")]);

    assert_eq!(row["partial"], true, "{row}");
    assert_eq!(row["views"]["count"], 1, "{row}");
    assert_eq!(row["views"]["measured"], 0, "{row}");
}

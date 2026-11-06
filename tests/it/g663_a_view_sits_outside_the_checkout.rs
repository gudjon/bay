//! G663 (a fleet finding, 2026-11-05: installs in a sparse view succeeded and wrote to the main
//! checkout). A view lived at `<checkout>/.git/bay/views/<id>`, so a tool walking up its parents
//! for a workspace file or a package reached the checkout, and agent products refused to edit a
//! path under `.git`.
//!
//! Now a new view sits under the bay state directory, whose parents hold no checkout. A view
//! already open stays where it is and works as before. A deleted clone's views cannot be judged
//! with their repository gone, so `bay doctor` names them and nothing removes them.
//!
//! Guards to mutate: `src/repo.rs` `views_root` (the first case fails) and `view_home`, the inner
//! directory (the open view is lost); `src/view.rs` `all`, the second directory (the merged view
//! is never released), `actor_at`, the state layout (G653's refusal fails) and `left_behind`;
//! `src/check.rs` `hidden_for`, the views directory, and `read_path_overlap`, the nearest existing
//! parent (G285's read paths over the views and over a scratch not made yet, on macOS).

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

/// Moves the view of `id` where bay put views before G663, with its journal.
fn open_before_the_move(test_box: &TestBox, repo: &Path, id: &str, view: &Path) -> PathBuf {
    let inner = repo.join(".git/bay/views");
    std::fs::create_dir_all(&inner).unwrap();
    let moved = inner.join(id);
    test_box.git_ok(
        repo,
        &[
            "worktree",
            "move",
            view.to_str().unwrap(),
            moved.to_str().unwrap(),
        ],
    );
    let journal = format!("{id}.journal");
    std::fs::rename(view.with_file_name(&journal), inner.join(&journal)).unwrap();
    moved
}

fn status_of(test_box: &TestBox, repo: &Path, id: &str) -> Value {
    let (_, status) = result_json(test_box.bay(repo).args(["status", id]));
    status["detail"]["changes"][0].clone()
}

#[test]
fn a_tool_walking_up_from_a_new_view_never_reaches_the_checkout() {
    let (test_box, repo) = seeded();
    std::fs::write(repo.join("workspace.marker"), "the checkout's\n").unwrap();

    let (_, view) = start(&test_box, &repo, "NEW");

    assert!(view.join("src/a.rs").is_file(), "{}", view.display());
    assert!(
        !view
            .ancestors()
            .any(|dir| dir.join("workspace.marker").exists()),
        "{} reaches the checkout's file",
        view.display()
    );
    assert!(
        !view.to_str().unwrap().contains("/.git/"),
        "{}",
        view.display()
    );
}

#[test]
fn a_view_opened_before_the_move_still_works_and_merges() {
    let (test_box, repo) = seeded();
    let (id, view) = start(&test_box, &repo, "OPEN");
    let moved = open_before_the_move(&test_box, &repo, &id, &view);

    let change = status_of(&test_box, &repo, &id);
    assert_eq!(change["change"], id.as_str(), "{change}");
    let (_, new_view) = start(&test_box, &repo, "NEW");
    assert_ne!(new_view.parent(), moved.parent());

    std::fs::write(moved.join("src/a.rs"), "fn a() { 1; }\n").unwrap();
    let (code, merged) = result_json(test_box.bay(&moved).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");

    // Cleanup finds it where it stays, as it does a new view.
    start(&test_box, &repo, "NEXT");
    assert!(!moved.exists(), "the merged view was not released");
}

#[test]
fn doctor_names_the_views_a_deleted_clone_left_behind_and_keeps_them() {
    let (test_box, repo) = seeded();
    let gone = test_box.repo_any("gone");
    test_box.seed_with_check(&gone, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let (_, view) = start(&test_box, &gone, "LEFT");
    let left = view.parent().unwrap().to_path_buf();
    std::fs::remove_dir_all(&gone).unwrap();

    let (_, doctor) = result_json(test_box.bay(&repo).args(["doctor"]));

    let rows = doctor["detail"]["rows"].as_array().expect("doctor rows");
    let store = rows.iter().find(|row| row["name"] == "store").unwrap();
    let named: Vec<&str> = store["views_left_behind"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(named, [left.to_str().unwrap()], "{store}");
    assert!(view.join("src/a.rs").is_file(), "the view must stay");
}

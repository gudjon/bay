//! G510 (bay 0.6.1 B3, five-whys item 10, 2026-10-29). `bay save <intent> --paths <path>...`
//! lands only the edits under those paths, from any checkout, and leaves the checkout as it was.
//!
//! Seats work in linked worktrees (seat homes), and `bay save` took every edit of the main
//! checkout only, then moved it, so there was no one-command way to land one folder. With
//! `--paths`, bay opens the change on exactly those paths, its draft holds only the edits under
//! them, and it merges it; it runs in a linked worktree as in the main checkout, and the
//! checkout's `HEAD`, branch, index and every file stay as they were, the saved ones too. An edit
//! outside the paths is never saved. A path with no edit under it is refused before anything is
//! made, never skipped. Saving the same paths again once main holds them saves nothing. Without
//! `--paths`, a linked worktree is still refused (G279): that save moves the main checkout's own
//! branch, which only the main checkout may do.
//!
//! The guards to mutate, in `src/save.rs`:
//! - `under_paths`: return `changed` whole (the first case fails: `src/a.rs` lands too).
//! - `under_paths`'s refusal of a path with no edit: drop it (the third case fails: the save
//!   goes on and merges `docs/` alone).
//! - `Checkout::of` with `--paths`: take the main checkout's root, as without it (the first case
//!   fails: the main checkout holds no edit under `docs/`, so the save is refused).
//! - `save_in`'s `stays(&head)` for `--paths`: move the checkout as without it (the second case
//!   fails: the main checkout's `HEAD` moves to the merge).
//! - `Checkout::of`'s `main_checkout_only` without `--paths`: drop it (the fourth case fails:
//!   the worktree's save takes the main checkout's edits and moves it).
//! - `save_in`'s `on_main_already` for `--paths`: drop it (the first case's second save opens
//!   a change that changes nothing).
//! - `in_progress` reads the checkout's own git directory: read `repo.common_dir` instead (the
//!   fifth case fails both ways).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json, unchecked_files};

const SEED: &[(&str, &str)] = &[
    ("docs/a.md", "a\n"),
    ("docs/b.md", "b\n"),
    ("src/a.rs", "fn a() {}\n"),
    ("src/b.rs", "fn b() {}\n"),
];

/// A trunk with [`SEED`] and no checks, and a linked worktree `seat` on a branch of its own at
/// trunk's tip, as a seat home stands. Returns (box, main checkout, seat, remote).
fn trunk_and_seat() -> (TestBox, PathBuf, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, SEED, &unchecked_files());
    let seat = test_box.path().join("seat");
    test_box.git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "seat",
            seat.to_str().unwrap(),
        ],
    );
    (test_box, repo, seat, remote)
}

fn write(dir: &Path, path: &str, text: &str) {
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

fn read(dir: &Path, path: &str) -> String {
    std::fs::read_to_string(dir.join(path)).unwrap()
}

/// What a checkout is: its `HEAD`, the ref it stands on, its index and its `git status`.
fn state(test_box: &TestBox, dir: &Path) -> Vec<String> {
    let on = test_box
        .git(dir)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .unwrap();
    vec![
        test_box.git_ok(dir, &["rev-parse", "HEAD"]),
        String::from_utf8(on.stdout).unwrap().trim().to_owned(),
        test_box.git_ok(dir, &["ls-files", "--stage"]),
        test_box.git_ok(dir, &["status", "--porcelain", "--untracked-files=all"]),
    ]
}

fn refs(test_box: &TestBox, remote: &Path, prefix: &str) -> Vec<String> {
    test_box
        .git_bare(
            remote,
            &["for-each-ref", "--format=%(objectname) %(refname)", prefix],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The paths change `id` declares, from its record.
fn declared(test_box: &TestBox, repo: &Path, id: &str) -> serde_json::Value {
    let record = test_box.git_ok(
        repo,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    record["declared_inputs"].clone()
}

#[test]
fn in_a_linked_worktree_only_the_edits_under_the_paths_land_and_it_stays_as_it_was() {
    let (test_box, repo, seat, remote) = trunk_and_seat();
    let before = test_box.git_bare(&remote, &["rev-parse", "main"]);
    write(&seat, "docs/a.md", "a, edited\n");
    write(&seat, "docs/new.md", "new\n");
    std::fs::remove_file(seat.join("docs/b.md")).unwrap();
    write(&seat, "src/a.rs", "fn a() { 1 }\n");
    write(&seat, "notes/mine.md", "not saved\n");
    // The main checkout's own edit is no part of the seat's save.
    write(&repo, "src/b.rs", "fn b() { 2 }\n");
    let (seat_was, main_was) = (state(&test_box, &seat), state(&test_box, &repo));

    let (code, saved) =
        result_json(
            test_box
                .bay(&seat)
                .args(["save", "CARD-docs", "--paths", "docs/"]),
        );
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    assert_eq!(
        saved["detail"]["saved"],
        serde_json::json!(["docs/a.md", "docs/b.md", "docs/new.md"]),
        "{saved}"
    );

    // Main holds one new commit with exactly the edits under docs/.
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main^"]), before);
    assert_eq!(
        test_box.git_bare(
            &remote,
            &["diff", "--name-status", "--no-renames", &before, "main"]
        ),
        "M\tdocs/a.md\nD\tdocs/b.md\nA\tdocs/new.md"
    );
    let id = saved["change"].as_str().expect("the change is named");
    assert_eq!(
        declared(&test_box, &repo, id),
        serde_json::json!(["docs/"]),
        "the change declares exactly the paths given"
    );

    // The seat and the main checkout are exactly as they were: HEAD, branch, index and files.
    assert_eq!(saved["detail"]["checkout"]["moved"], false, "{saved}");
    assert_eq!(state(&test_box, &seat), seat_was);
    assert_eq!(state(&test_box, &repo), main_was);
    assert_eq!(read(&seat, "docs/a.md"), "a, edited\n");
    assert_eq!(read(&seat, "src/a.rs"), "fn a() { 1 }\n");
    assert_eq!(read(&seat, "notes/mine.md"), "not saved\n");
    assert_eq!(read(&repo, "src/b.rs"), "fn b() { 2 }\n");

    // The same paths again, with main holding them: nothing to save, nothing made.
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let changes = refs(&test_box, &remote, "refs/bay/changes/");
    let (code, again) =
        result_json(
            test_box
                .bay(&seat)
                .args(["save", "CARD-docs", "--paths", "docs/"]),
        );
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["decision"], "ok", "{again}");
    let why = again["detail"]["why"].as_str().unwrap_or_default();
    assert!(why.starts_with("nothing to save"), "{again}");
    assert_eq!(again["state"], "no_effect", "{again}");
    assert_eq!(
        again["say"], "Nothing new to add; main already has these edits.",
        "{again}"
    );
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);
    assert_eq!(refs(&test_box, &remote, "refs/bay/changes/"), changes);
    assert_eq!(state(&test_box, &seat), seat_was);
}

#[test]
fn in_the_main_checkout_paths_land_only_those_edits_and_head_stays() {
    let (test_box, repo, _seat, remote) = trunk_and_seat();
    let before = test_box.git_bare(&remote, &["rev-parse", "main"]);
    write(&repo, "docs/a.md", "a, edited\n");
    // An untracked file outside the paths: never saved, and no reason of its own to stay.
    write(&repo, "notes/mine.md", "not saved\n");
    let was = state(&test_box, &repo);

    let (code, saved) =
        result_json(
            test_box
                .bay(&repo)
                .args(["save", "CARD-a", "--paths", "docs/a.md"]),
        );
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    assert_eq!(
        test_box.git_bare(
            &remote,
            &["diff", "--name-status", "--no-renames", &before, "main"]
        ),
        "M\tdocs/a.md"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:docs/a.md"]),
        "a, edited"
    );
    // HEAD stays where it was: the main checkout does not follow the merge.
    assert_eq!(saved["detail"]["checkout"]["moved"], false, "{saved}");
    assert_eq!(state(&test_box, &repo), was);
    assert_eq!(test_box.git_ok(&repo, &["rev-parse", "HEAD"]), before);
    assert_eq!(read(&repo, "notes/mine.md"), "not saved\n");
}

#[test]
fn a_path_with_no_edit_is_refused_and_nothing_is_made() {
    let (test_box, repo, seat, remote) = trunk_and_seat();
    write(&seat, "docs/a.md", "a, edited\n");
    let was = state(&test_box, &seat);
    let changes = refs(&test_box, &remote, "refs/bay/changes/");
    let claims = refs(&test_box, &remote, "refs/bay/intent/");
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);

    let (code, refused) =
        result_json(
            test_box
                .bay(&seat)
                .args(["save", "CARD-none", "--paths", "docs/", "src/"]),
        );
    assert_ne!(code, 0, "{refused}");
    let why = refused["detail"].as_str().unwrap_or_default();
    assert!(
        why.contains("src/"),
        "names the path with no edit: {refused}"
    );
    assert!(!why.contains("docs/"), "names only that path: {refused}");
    assert_eq!(refs(&test_box, &remote, "refs/bay/changes/"), changes);
    assert_eq!(refs(&test_box, &remote, "refs/bay/intent/"), claims);
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);
    assert_eq!(state(&test_box, &seat), was);

    // In a view, --paths is refused as a plain save is, and names bay merge.
    let started = test_box.bay_ok(&repo, &["start", "CARD-view", "--paths", "docs/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    write(&view, "docs/a.md", "in the view\n");
    let (code, refused) = result_json(
        test_box
            .bay(&view)
            .args(["save", "CARD-v", "--paths", "docs/"]),
    );
    assert_ne!(code, 0, "{refused}");
    let why = refused["detail"].as_str().unwrap_or_default();
    assert!(why.contains("bay merge"), "names bay merge: {refused}");
}

#[test]
fn without_paths_a_linked_worktree_is_refused_and_named_to_paths() {
    let (test_box, repo, seat, remote) = trunk_and_seat();
    write(&seat, "docs/a.md", "a, edited\n");
    write(&repo, "src/b.rs", "fn b() { 2 }\n");
    let (seat_was, main_was) = (state(&test_box, &seat), state(&test_box, &repo));
    let changes = refs(&test_box, &remote, "refs/bay/changes/");
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);

    let (code, refused) = result_json(test_box.bay(&seat).args(["save", "CARD-all"]));
    assert_ne!(code, 0, "{refused}");
    let why = refused["detail"].as_str().unwrap_or_default();
    assert!(why.contains("main checkout"), "{refused}");
    assert!(why.contains("--paths"), "names --paths: {refused}");
    assert_eq!(refs(&test_box, &remote, "refs/bay/changes/"), changes);
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);
    assert_eq!(state(&test_box, &seat), seat_was);
    assert_eq!(state(&test_box, &repo), main_was);
}

#[test]
fn a_merge_in_progress_stops_only_the_checkout_it_is_in() {
    let (test_box, repo, seat, remote) = trunk_and_seat();
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    write(&seat, "docs/a.md", "a, edited\n");
    // The seat's own merge in progress refuses its save.
    let seat_dir = PathBuf::from(test_box.git_ok(&seat, &["rev-parse", "--absolute-git-dir"]));
    std::fs::write(seat_dir.join("MERGE_HEAD"), format!("{tip}\n")).unwrap();
    let (code, refused) = result_json(
        test_box
            .bay(&seat)
            .args(["save", "CARD-m", "--paths", "docs/"]),
    );
    assert_ne!(code, 0, "{refused}");
    let why = refused["detail"].as_str().unwrap_or_default();
    assert!(why.contains("in the middle of a merge"), "{refused}");
    std::fs::remove_file(seat_dir.join("MERGE_HEAD")).unwrap();

    // The main checkout's merge in progress is no part of the seat's save.
    let main_dir = PathBuf::from(test_box.git_ok(&repo, &["rev-parse", "--absolute-git-dir"]));
    std::fs::write(main_dir.join("MERGE_HEAD"), format!("{tip}\n")).unwrap();
    let (code, saved) = result_json(
        test_box
            .bay(&seat)
            .args(["save", "CARD-m", "--paths", "docs/"]),
    );
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
}

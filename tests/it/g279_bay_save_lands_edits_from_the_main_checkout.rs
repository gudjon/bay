//! G279. `bay save` lands the edits made in the main checkout.
//!
//! People who are not developers edit files directly in the repository's main checkout and
//! never run `bay start` (owner ruling 2026-10-21). `bay save <intent>` collects the working
//! tree's changes against `HEAD` (modified, added untracked files that are not ignored, and
//! deleted files), opens the normal change on exactly those files, whose draft is `HEAD` plus
//! the edits merged onto the current trunk the way `start --from` takes a branch in, and lands
//! it with `bay merge --wait`. On `landed` the main checkout's trunk branch fast-forwards to the
//! landing and its files follow, so it is clean at the new tip; an ignored file stays. A
//! conflict with newer trunk is `MERGE_CONFLICT` before anything is created; a failing check is
//! `CHECK_FAILED` naming the change, which stays open for repair. On every refusal the main
//! checkout's files and `HEAD` are exactly as they were. In a view, `bay save` is refused and
//! names `bay merge`.
//!
//! The guards to mutate:
//! - `src/start.rs`, the draft the new change starts from in `create`,
//!   `let draft = taken_in.map_or(base, |taken_in| taken_in.draft.as_str());` (make it
//!   `let draft = base;` and the first case fails: trunk does not get the edits).
//! - `src/start.rs`, `take_saved`'s `merge_onto(&repo.git, base, &saved.commit)`: commit the
//!   saved commit's own tree on `base` instead and the second case fails (trunk's newer file is
//!   dropped); its `Merge::Conflict` arm, made to commit the conflicted tree, fails the third.
//! - `src/save.rs`, `save_in`'s `Some(commit) => bring_checkout(...)`: make it
//!   `json!({"moved": false})` and the first case fails (the checkout stays at the old `HEAD`).
//! - `src/save.rs`, `save_in`'s `if landed.decision != Decision::Landed` return of
//!   `untouched(landed, &saved)`: drop it and the fourth case fails (no `repair` names the
//!   change).
//! - `src/save.rs`, `save_in`'s `if let Some(change) = repo.view_change(dir)` refusal: drop it
//!   and the fifth case fails (the refusal no longer names `bay merge`).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json, unchecked_files};

const SEED: &[(&str, &str)] = &[
    ("src/a.rs", "fn a() {}\n"),
    ("src/b.rs", "fn b() {}\n"),
    ("src/c.rs", "fn c() {}\n"),
    ("src/x.rs", "one\ntwo\n"),
    (".gitignore", "*.log\n"),
];

/// A trunk with [`SEED`] and no checks. Returns (box, main checkout, remote).
fn trunk() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, SEED, &unchecked_files());
    (test_box, repo, remote)
}

fn write(dir: &Path, path: &str, text: &str) {
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

fn read(dir: &Path, path: &str) -> String {
    std::fs::read_to_string(dir.join(path)).unwrap()
}

/// Lands one change through bay that writes `path` in its view, so trunk moves past the main
/// checkout's `HEAD`.
fn land_through_bay(test_box: &TestBox, repo: &Path, intent: &str, path: &str, text: &str) {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    write(&view, path, text);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
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

/// The main checkout's `git status --porcelain`, with every untracked file.
fn status(test_box: &TestBox, repo: &Path) -> String {
    test_box.git_ok(repo, &["status", "--porcelain", "--untracked-files=all"])
}

#[test]
fn edits_additions_and_deletions_in_the_main_checkout_land_as_one_change() {
    let (test_box, repo, remote) = trunk();
    let before = test_box.git_bare(&remote, &["rev-parse", "main"]);
    write(&repo, "src/a.rs", "fn a() { 1 }\n");
    write(&repo, "src/b.rs", "fn b() { 2 }\n");
    write(&repo, "src/new.rs", "fn new() {}\n");
    std::fs::remove_file(repo.join("src/c.rs")).unwrap();
    // An ignored file is never saved, and the save leaves it alone.
    write(&repo, "notes.log", "my notes\n");

    let (code, saved) = result_json(test_box.bay(&repo).args(["save", "CARD-1"]));
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    assert_eq!(
        saved["detail"]["saved"],
        serde_json::json!(["src/a.rs", "src/b.rs", "src/c.rs", "src/new.rs"]),
        "{saved}"
    );

    // Trunk holds one new commit on the old tip, with exactly those changes.
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    assert_eq!(
        test_box.git_bare(
            &remote,
            &["rev-list", "--count", &format!("{before}..main")]
        ),
        "1"
    );
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main^"]), before);
    assert_eq!(
        test_box.git_bare(
            &remote,
            &["diff", "--name-status", "--no-renames", &before, "main"]
        ),
        "M\tsrc/a.rs\nM\tsrc/b.rs\nD\tsrc/c.rs\nA\tsrc/new.rs"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/a.rs"]),
        "fn a() { 1 }"
    );
    let id = test_box.trunk_change_ids(&remote)[0].clone();
    assert_eq!(saved["change"], id.as_str(), "{saved}");
    assert_eq!(saved["detail"]["change"], id.as_str(), "{saved}");
    assert_eq!(saved["detail"]["landed"], tip.as_str(), "{saved}");

    // The main checkout is clean at the new tip, by a fast-forward of its own branch.
    assert_eq!(saved["detail"]["checkout"]["moved"], true, "{saved}");
    assert_eq!(test_box.git_ok(&repo, &["rev-parse", "HEAD"]), tip);
    assert_eq!(
        test_box.git_ok(&repo, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(status(&test_box, &repo), "", "the checkout is clean");
    assert_eq!(read(&repo, "src/new.rs"), "fn new() {}\n");
    assert!(!repo.join("src/c.rs").exists());
    assert_eq!(read(&repo, "notes.log"), "my notes\n");
    assert!(
        test_box
            .git(test_box.path())
            .arg(format!("--git-dir={}", remote.display()))
            .args(["cat-file", "-e", "main:notes.log"])
            .status()
            .is_ok_and(|status| !status.success()),
        "an ignored file never reaches trunk"
    );

    // A second save with nothing changed lands nothing.
    let again = test_box.bay_ok(&repo, &["save", "CARD-1"]);
    assert_eq!(again["decision"], "ok", "{again}");
    assert_eq!(again["detail"]["why"], "nothing to save", "{again}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);
}

#[test]
fn a_save_after_trunk_moved_on_another_file_lands_merged() {
    let (test_box, repo, remote) = trunk();
    let head = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    land_through_bay(
        &test_box,
        &repo,
        "CARD-other",
        "src/other.rs",
        "fn other() {}\n",
    );
    let moved = test_box.git_bare(&remote, &["rev-parse", "main"]);
    assert_ne!(moved, head, "trunk moved past the main checkout's HEAD");
    write(&repo, "src/a.rs", "fn a() { 1 }\n");

    let (code, saved) =
        result_json(
            test_box
                .bay(&repo)
                .args(["save", "CARD-2", "--message", "a is one now"]),
        );
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    assert_eq!(
        saved["detail"]["saved"],
        serde_json::json!(["src/a.rs"]),
        "{saved}"
    );
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main^"]), moved);
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/a.rs"]),
        "fn a() { 1 }"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/other.rs"]),
        "fn other() {}",
        "trunk's newer file is kept"
    );
    // The main checkout fast-forwards past both landings, and holds trunk's newer file.
    assert_eq!(test_box.git_ok(&repo, &["rev-parse", "HEAD"]), tip);
    assert_eq!(status(&test_box, &repo), "", "the checkout is clean");
    assert_eq!(read(&repo, "src/other.rs"), "fn other() {}\n");
    assert_eq!(read(&repo, "src/a.rs"), "fn a() { 1 }\n");
}

#[test]
fn a_save_that_conflicts_with_trunk_is_merge_conflict_and_creates_nothing() {
    let (test_box, repo, remote) = trunk();
    let head = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    land_through_bay(&test_box, &repo, "CARD-trunk", "src/x.rs", "TRUNK\ntwo\n");
    write(&repo, "src/x.rs", "MINE\ntwo\n");
    write(&repo, "src/new.rs", "fn new() {}\n");
    let changes = refs(&test_box, &remote, "refs/bay/changes/");
    let claims = refs(&test_box, &remote, "refs/bay/intent/");
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let dirty = status(&test_box, &repo);

    let (code, refused) = result_json(test_box.bay(&repo).args(["save", "CARD-3"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "MERGE_CONFLICT", "{refused}");
    assert_eq!(
        refused["detail"]["conflicts"],
        serde_json::json!(["src/x.rs"]),
        "{refused}"
    );
    assert!(refused["change"].is_null(), "no change: {refused}");
    assert_eq!(refs(&test_box, &remote, "refs/bay/changes/"), changes);
    assert_eq!(refs(&test_box, &remote, "refs/bay/intent/"), claims);
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);

    // The edits are intact, and HEAD did not move.
    assert_eq!(read(&repo, "src/x.rs"), "MINE\ntwo\n");
    assert_eq!(read(&repo, "src/new.rs"), "fn new() {}\n");
    assert_eq!(test_box.git_ok(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(status(&test_box, &repo), dirty);
}

#[test]
fn a_save_whose_check_fails_is_check_failed_names_the_change_and_keeps_the_edits() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let trunk = test_box.seed_with_check(
        &repo,
        SEED,
        &check_files(
            &format!("[ -e src/broken ] && {{ {failing}; exit 0; }}"),
            &passing_result(1),
            1,
            60,
        ),
    );
    write(&repo, "src/a.rs", "fn a() { 1 }\n");
    write(&repo, "src/broken", "this fails the check\n");
    let dirty = status(&test_box, &repo);

    let (code, refused) = result_json(test_box.bay(&repo).args(["save", "CARD-4"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    let id = refused["change"].as_str().expect("the change is named");
    assert_eq!(refused["detail"]["change"], id, "{refused}");
    let repair = refused["detail"]["repair"].as_str().unwrap_or_default();
    assert!(repair.contains(&format!("bay start {id}")), "{refused}");
    assert_eq!(
        refused["detail"]["saved"],
        serde_json::json!(["src/a.rs", "src/broken"]),
        "{refused}"
    );

    // Trunk did not move, and the main checkout is exactly as it was.
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
    assert_eq!(test_box.git_ok(&repo, &["rev-parse", "HEAD"]), trunk);
    assert_eq!(read(&repo, "src/a.rs"), "fn a() { 1 }\n");
    assert_eq!(read(&repo, "src/broken"), "this fails the check\n");
    assert_eq!(status(&test_box, &repo), dirty);

    // The change stays open: its view resumes for repair.
    let resumed = test_box.bay_ok(&repo, &["start", id]);
    let view = PathBuf::from(resumed["detail"]["view"].as_str().unwrap());
    assert_eq!(read(&view, "src/a.rs"), "fn a() { 1 }\n");
}

#[test]
fn bay_save_in_a_view_is_refused_and_names_bay_land() {
    let (test_box, repo, remote) = trunk();
    let started = test_box.bay_ok(&repo, &["start", "CARD-view", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    write(&view, "src/a.rs", "fn a() { 1 }\n");
    let changes = refs(&test_box, &remote, "refs/bay/changes/");
    let claims = refs(&test_box, &remote, "refs/bay/intent/");
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);

    let (code, refused) = result_json(test_box.bay(&view).args(["save", "CARD-5"]));
    assert_ne!(code, 0, "{refused}");
    let why = refused["detail"].as_str().unwrap_or_default();
    assert!(why.contains("bay merge"), "names bay merge: {refused}");
    assert_eq!(refs(&test_box, &remote, "refs/bay/changes/"), changes);
    assert_eq!(refs(&test_box, &remote, "refs/bay/intent/"), claims);
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), tip);
    assert_eq!(read(&view, "src/a.rs"), "fn a() { 1 }\n");
}

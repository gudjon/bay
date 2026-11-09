//! G688 (a fleet finding, 2026-11-09: a `bay merge --from <branch>` whose branch added nothing to
//! main ended `no_effect`; the branch moved on, and the next `bay merge --from <branch>` took the
//! ended change up again with its old draft, so it ended `no_effect` once more and the branch's
//! new commits never landed). A start that takes in a branch, when the intent's change ended
//! without landing, opens the next generation from the branch's tip, as `--new` would; any start
//! does once the change ended with nothing to add, since that change can never merge. And a
//! merge of a change that has ended is refused in plain words, never a raw Git error about its
//! removed folder.
//!
//! Guards to mutate:
//! - `src/start.rs` `opens_next`: `request.from.is_some()` (make it `false`), with the
//!   abandoned change is taken up again by the second `merge --from`, and the branch's new
//!   file never lands.
//! - `src/start.rs` `opens_next`: `state == ChangeState::NoEffect` (make it `false`): the second
//!   case's plain start takes up the ended change, which can never merge.
//! - `src/publish.rs` `submit_land`: the `ended` refusal (the ended change is submitted again,
//!   or, once bay removed its view, fails with Git's "cannot change to" that folder).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json, unchecked_files};

/// Commits `files` on the remote's branch `outside` (made from `from`), and deletes it here.
fn push_outside(test_box: &TestBox, repo: &Path, from: &str, files: &[(&str, &str)]) {
    test_box.git_ok(repo, &["switch", "-q", "-C", "outside", from]);
    for (path, text) in files {
        let full = repo.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, text).unwrap();
    }
    test_box.git_ok(repo, &["add", "--all"]);
    test_box.git_ok(repo, &["commit", "-q", "-m", "outside"]);
    test_box.git_ok(repo, &["push", "-q", "-f", "origin", "outside"]);
    test_box.git_ok(repo, &["switch", "-q", "main"]);
    test_box.git_ok(repo, &["branch", "-q", "-D", "outside"]);
}

#[test]
fn a_branch_taken_in_again_after_its_change_ended_is_a_new_change() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let seed = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);

    // GIVEN a branch whose one edit main already has, so its merge ends no_effect
    let started = test_box.bay_ok(&repo, &["start", "CARD-first", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/b.rs"), "fn b() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    push_outside(&test_box, &repo, &seed, &[("src/b.rs", "fn b() {}\n")]);
    let (code, first) = result_json(
        test_box
            .bay(&repo)
            .args(["merge", "--from", "outside", "--wait"]),
    );
    assert_eq!(code, 0, "{first}");
    assert_eq!(first["state"], "no_effect", "{first}");
    let ended = first["change"].as_str().unwrap().to_owned();

    // WHEN the ended change is merged again by its id
    let (code, again) = result_json(test_box.bay(&repo).args(["merge", &ended]));
    // THEN it is refused in plain words
    assert_ne!(code, 0, "{again}");
    let said = again["say"].as_str().unwrap_or_default();
    assert!(!said.contains("cannot change to"), "{again}");
    assert_eq!(again["reason"], "CHANGE_ALREADY_MERGED", "{again}");

    // AND it is given up
    let (code, abandoned) = result_json(test_box.bay(&repo).args(["abandon", &ended]));
    assert_eq!(code, 0, "{abandoned}");

    // WHEN the branch moves on and is taken in again
    let main = test_box.git_bare(&remote, &["rev-parse", "main"]);
    push_outside(&test_box, &repo, &main, &[("src/c.rs", "fn c() {}\n")]);
    let (code, second) = result_json(
        test_box
            .bay(&repo)
            .args(["merge", "--from", "outside", "--wait"]),
    );

    // THEN a new change lands the branch's tip
    assert_eq!(code, 0, "{second}");
    assert_eq!(second["decision"], "merged", "{second}");
    let next = second["change"].as_str().unwrap();
    assert_ne!(next, ended, "the ended change was taken up again: {second}");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/c.rs"]),
        "fn c() {}"
    );
}

#[test]
fn a_start_after_a_change_ended_with_nothing_to_add_opens_a_new_change() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());

    // GIVEN a change that ended no_effect: another landed the same edit first
    let ended = test_box.bay_ok(&repo, &["start", "CARD-same", "--paths", "src/"]);
    let ended_view = PathBuf::from(ended["detail"]["view"].as_str().unwrap());
    let ended = ended["change"].as_str().unwrap().to_owned();
    let other = test_box.bay_ok(&repo, &["start", "CARD-other", "--paths", "src/"]);
    let other_view = PathBuf::from(other["detail"]["view"].as_str().unwrap());
    let mut landed = serde_json::Value::Null;
    for view in [&other_view, &ended_view] {
        std::fs::write(view.join("src/b.rs"), "fn b() {}\n").unwrap();
        let code;
        (code, landed) = result_json(test_box.bay(view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{landed}");
    }
    assert_eq!(landed["state"], "no_effect", "{landed}");

    // WHEN its intent is started again
    let again = test_box.bay_ok(&repo, &["start", "CARD-same", "--paths", "src/"]);

    // THEN a new change opens, with a view to work in
    assert_ne!(again["change"], ended.as_str(), "{again}");
    assert!(
        PathBuf::from(again["detail"]["view"].as_str().unwrap()).is_dir(),
        "{again}"
    );
}

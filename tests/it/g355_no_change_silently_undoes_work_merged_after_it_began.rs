//! G355 (owner ruling 2026-10-24, BACKLOG 64 F1): no change silently undoes work merged after it
//! began.
//!
//! A fleet lost hours to a merge that put back an older copy of a file: an agent brought in main,
//! then wrote the file from what it had read before, by a shell command the hook cannot judge,
//! and the merge was clean. Now the candidate is refused `UNDOES_MERGED` when it holds a file as
//! it was before another change main merged after this one began, naming that change, unless the
//! change says it undoes it (`bay undo`, `Reverts:`). Exact reversal only: reworking the same
//! lines merges, and so does a change that leaves alone a file main itself put back.
//!
//! Guards to mutate:
//! - `src/publish.rs` `undoes_merged`: return `Ok(None)` at once (the older copy merges and main
//!   loses the other change's line).
//! - the same: drop `&& !applies(&main_envs)` (a change that leaves alone a line main put back
//!   itself is refused).

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, UNCHECKED_POLICY, result_json};

/// `src/a.rs` on main: ten lines, so an edit of line 2 and one of line 9 touch apart.
const A: &str = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";

/// `A` with line 2 as `two` and line 9 as `nine`, and `extra` after it.
fn a(two: &str, nine: &str, extra: &str) -> String {
    format!("l1\n{two}\nl3\nl4\nl5\nl6\nl7\nl8\n{nine}\nl10\n{extra}")
}

/// A repository whose main holds `src/a.rs` as [`A`], with no check; and another clone.
fn seeded(test_box: &TestBox) -> (PathBuf, PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", A)],
        &[(".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned())],
    );
    let remote = test_box.path().join("repo-remote.git");
    let other = test_box.clone_of(&remote, "other");
    (repo, other, remote)
}

/// Saves `text` as `file` from the clone `clone` (with a pull first), as one merged change.
fn merges(test_box: &TestBox, clone: &Path, file: &str, text: &str, intent: &str) -> String {
    test_box.git_ok(clone, &["pull", "-q", "--ff-only"]);
    std::fs::write(clone.join(file), text).unwrap();
    let (code, saved) = result_json(test_box.bay(clone).args(["save", intent]));
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    saved["change"].as_str().unwrap().to_owned()
}

/// A change opened in `repo` on `src/`: its id and view.
fn opened(test_box: &TestBox, repo: &Path, intent: &str) -> (String, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    (
        started["change"].as_str().unwrap().to_owned(),
        PathBuf::from(started["detail"]["view"].as_str().unwrap()),
    )
}

#[test]
fn an_older_copy_written_after_bringing_in_main_is_refused() {
    let test_box = TestBox::new();
    let (repo, other, remote) = seeded(&test_box);
    let (id, view) = opened(&test_box, &repo, "add four");
    let theirs = merges(
        &test_box,
        &other,
        "src/a.rs",
        &a("L2", "l9", ""),
        "shout two",
    );
    // The change brings in main, then writes the file from the copy it read before.
    test_box.bay_ok(&repo, &["start", &id]);
    std::fs::write(view.join("src/a.rs"), a("l2", "l9", "l11\n")).unwrap();
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "UNDOES_MERGED", "{refused}");
    assert_eq!(refused["detail"]["change"], theirs.as_str(), "{refused}");
    assert_eq!(refused["detail"]["paths"][0], "src/a.rs", "{refused}");
    assert!(
        test_box
            .git_bare(&remote, &["show", "main:src/a.rs"])
            .contains("L2")
    );
    // Kept on the newest copy, the same change merges.
    std::fs::write(view.join("src/a.rs"), a("L2", "l9", "l11\n")).unwrap();
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
}

#[test]
fn reworking_the_lines_or_leaving_a_file_main_put_back_merges() {
    let test_box = TestBox::new();
    let (repo, other, _remote) = seeded(&test_box);
    let (id, view) = opened(&test_box, &repo, "rework two");
    let (_, beside) = opened(&test_box, &repo, "shout nine");
    merges(
        &test_box,
        &other,
        "src/a.rs",
        &a("L2", "l9", ""),
        "shout two",
    );
    // Main puts the line back itself, by another change.
    merges(&test_box, &other, "src/a.rs", A, "quiet two");
    // A change that edits line 9 only merges, though line 2 is as it was before "shout two":
    // main itself put it back.
    std::fs::write(beside.join("src/a.rs"), a("l2", "L9", "")).unwrap();
    let (code, merged) = result_json(test_box.bay(&beside).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    // A change that reworks the line itself, on the newest main, merges.
    test_box.bay_ok(&repo, &["start", &id]);
    std::fs::write(view.join("src/a.rs"), a("l2 reworked", "L9", "")).unwrap();
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
}

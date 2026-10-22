//! G156. `bay status --heads` names work that exists only on this disk.
//!
//! A local branch whose tip no remote-tracking ref contains holds commits no remote has: the
//! list names it with the commits it is ahead, the age of its tip and the worktree it is
//! checked out in. A worktree record whose folder is gone is listed too, in `detail.stranded`
//! (0.3.0: `bay status --stranded` is gone; `--heads` shows every head). That part of the read
//! uses local refs only, and nothing is pruned or deleted.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::{TestBox, result_json};

fn commit(test_box: &TestBox, dir: &std::path::Path, file: &str, date: &str) {
    std::fs::write(dir.join(file), format!("{file}\n")).unwrap();
    test_box.git_ok(dir, &["add", file]);
    let out = test_box
        .git(dir)
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_AUTHOR_DATE", date)
        .args(["commit", "-q", "-m", file])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
}

fn branch<'a>(result: &'a Value, name: &str) -> Option<&'a Value> {
    result["detail"]["stranded"]["branches"]
        .as_array()
        .unwrap_or_else(|| panic!("no branches: {result}"))
        .iter()
        .find(|entry| entry["branch"] == name)
}

#[test]
fn branches_no_remote_holds_and_worktrees_whose_folder_is_gone_are_listed() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);

    // Held: pushed, so its remote-tracking ref contains it.
    test_box.git_ok(&repo, &["switch", "-q", "-c", "held"]);
    commit(&test_box, &repo, "held.txt", "2026-01-01T00:00:00Z");
    test_box.git_ok(&repo, &["push", "-q", "origin", "held"]);
    // Behind trunk only: its tip is on the remote's trunk.
    test_box.git_ok(&repo, &["branch", "old", "main"]);
    // Two commits only this disk has, checked out in a worktree of its own.
    test_box.git_ok(&repo, &["switch", "-q", "-c", "local-only", "main"]);
    commit(&test_box, &repo, "one.txt", "2020-01-01T00:00:00Z");
    commit(&test_box, &repo, "two.txt", "2020-01-02T00:00:00Z");
    test_box.git_ok(&repo, &["switch", "-q", "main"]);
    let worktree = test_box.path().join("wt");
    test_box.git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            worktree.to_str().unwrap(),
            "local-only",
        ],
    );
    // A worktree record whose folder is gone.
    let gone = test_box.path().join("gone");
    test_box.git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "gone-branch",
            gone.to_str().unwrap(),
            "main",
        ],
    );
    std::fs::remove_dir_all(&gone).unwrap();
    let refs_before = test_box.git_ok(&repo, &["for-each-ref"]);

    let (code, result) = result_json(test_box.bay(&repo).args(["status", "--heads"]));
    assert_eq!(code, 0, "{result}");
    let stranded = branch(&result, "local-only").unwrap_or_else(|| panic!("{result}"));
    assert_eq!(stranded["ahead"], 2, "{stranded}");
    assert!(
        stranded["age_s"]
            .as_u64()
            .is_some_and(|age| age > 365 * 24 * 3600),
        "{stranded}"
    );
    let path = stranded["worktree"]
        .as_str()
        .unwrap_or_else(|| panic!("{stranded}"));
    assert_eq!(
        std::fs::canonicalize(path).unwrap(),
        std::fs::canonicalize(&worktree).unwrap(),
        "{stranded}"
    );
    for held in ["main", "held", "old", "gone-branch"] {
        assert!(
            branch(&result, held).is_none(),
            "{held} is on the remote: {result}"
        );
    }
    let prunable = result["detail"]["stranded"]["prunable_worktrees"]
        .as_array()
        .unwrap_or_else(|| panic!("{result}"));
    assert_eq!(prunable.len(), 1, "{result}");
    assert!(
        prunable[0]["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("/gone")),
        "{result}"
    );
    assert_eq!(prunable[0]["branch"], "gone-branch", "{result}");

    // Read only: every ref and the worktree record are as they were.
    assert_eq!(test_box.git_ok(&repo, &["for-each-ref"]), refs_before);
    let records = test_box.git_ok(&repo, &["worktree", "list", "--porcelain"]);
    assert!(records.contains("prunable"), "{records}");
}

#[test]
fn heads_conflicts_with_a_change_and_with_fleet() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    for args in [
        &["status", "--heads", "--fleet"][..],
        &["status", "abcdefgh", "--heads"][..],
    ] {
        let out = test_box.bay(&repo).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
    }
}

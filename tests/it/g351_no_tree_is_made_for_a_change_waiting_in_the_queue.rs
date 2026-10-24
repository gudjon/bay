//! G351 (owner ruling 2026-10-24, BACKLOG 67 item 6): no tree is made for a change waiting in
//! the queue.
//!
//! A fleet's merge queue made each run's tree when the run joined the queue: 21 waiting trees
//! held 41 GB of a box's disk. bay makes a candidate's check tree only when its check runs
//! (`<git-common-dir>/bay/check/<candidate>`) and removes it when the check ends. So changes
//! waiting in the queue hold no tree and no worktree, and after the runner merges them none is
//! left.
//!
//! Guard to mutate: `src/publish.rs`, the check step: drop the `remove_dir_all` of the checks'
//! scratch (each merged change leaves its tree behind).

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// The folders under `<repo>/.git/bay/check`; none when it does not exist.
fn check_trees(repo: &Path) -> Vec<String> {
    std::fs::read_dir(repo.join(".git/bay/check"))
        .map(|dir| {
            dir.filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The worktrees `git worktree list` names in `repo`.
fn worktrees(test_box: &TestBox, repo: &Path) -> usize {
    test_box
        .git_ok(repo, &["worktree", "list", "--porcelain"])
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count()
}

#[test]
fn queued_changes_hold_no_tree_and_merged_ones_leave_none() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let remote = test_box.path().join("repo-remote.git");
    let runner = test_box.clone_of(&remote, "runner");

    let mut views: Vec<PathBuf> = Vec::new();
    for n in 1..=3 {
        let started = test_box.bay_ok(&repo, &["start", &format!("file {n}"), "--paths", "src/"]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(format!("src/f{n}.rs")), "fn f() {}\n").unwrap();
        views.push(view);
    }
    let before = worktrees(&test_box, &repo);
    for view in &views {
        let (code, queued) = result_json(test_box.bay(view).args(["merge"]));
        assert_eq!(code, 0, "{queued}");
        assert_eq!(queued["decision"], "queued", "{queued}");
    }
    assert_eq!(
        check_trees(&repo),
        Vec::<String>::new(),
        "a waiting change made a tree"
    );
    assert_eq!(
        check_trees(&runner),
        Vec::<String>::new(),
        "a waiting change made a tree"
    );
    assert_eq!(
        worktrees(&test_box, &repo),
        before,
        "a waiting change made a worktree"
    );

    // The runner checks and merges them; each check's tree goes when its check ends.
    for _ in 0..6 {
        test_box.bay_ok(&runner, &["runner", "run", "--once"]);
        let main = test_box.git_bare(&remote, &["ls-tree", "--name-only", "main", "src/"]);
        if (1..=3).all(|n| main.contains(&format!("src/f{n}.rs"))) {
            break;
        }
    }
    let main = test_box.git_bare(&remote, &["ls-tree", "--name-only", "main", "src/"]);
    assert!(
        (1..=3).all(|n| main.contains(&format!("src/f{n}.rs"))),
        "the runner merged them all: {main}"
    );
    assert_eq!(
        check_trees(&runner),
        Vec::<String>::new(),
        "a merged change left its tree"
    );
}

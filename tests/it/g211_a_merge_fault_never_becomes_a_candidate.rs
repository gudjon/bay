//! G211: a merge fault never becomes a candidate (`DESIGN` § 6 step 2).
//!
//! `git merge-tree` exits 0 for a clean merge and 1 for a conflict. Any other exit is a fault:
//! bay returns an error and builds no candidate. It never retries on another base, and never
//! passes `--allow-unrelated-histories`, so a draft that shares no history with trunk cannot
//! replace trunk's newer content.

use std::fmt::Write as _;

use crate::support;

use bay::git::Git;
use bay::merge::{Merge, merge_onto};
use support::TestBox;

/// `cmd` with `input` on stdin; its stdout.
fn run_with_stdin(cmd: &mut std::process::Command, input: &str) -> String {
    use std::io::Write;
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{cmd:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// A commit of `files` (name, text) on `parents`, made without touching any branch.
fn commit(
    test_box: &TestBox,
    repo: &std::path::Path,
    files: &[(&str, &str)],
    parents: &[&str],
) -> String {
    let mut entries = String::new();
    for (name, text) in files {
        let mut cmd = test_box.git(repo);
        cmd.args(["hash-object", "-w", "--stdin"]);
        let blob = run_with_stdin(&mut cmd, text);
        writeln!(entries, "100644 blob {}\t{name}", blob.trim()).unwrap();
    }
    let tree = run_with_stdin(test_box.git(repo).arg("mktree"), &entries)
        .trim()
        .to_owned();
    let mut args = vec!["commit-tree", tree.as_str(), "-m", files[0].0];
    for parent in parents {
        args.extend(["-p", parent]);
    }
    test_box.git_ok(repo, &args)
}

#[test]
fn a_draft_with_no_common_history_is_a_fault_never_a_clean_merge() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let root = commit(&test_box, &repo, &[("a.txt", "one\n")], &[]);
    let trunk = commit(&test_box, &repo, &[("a.txt", "two\n")], &[&root]);
    let orphan = commit(&test_box, &repo, &[("b.txt", "other\n")], &[]);
    let git = Git::new(&repo);

    // The related case still merges, so the fault below is about the history alone.
    let related = commit(
        &test_box,
        &repo,
        &[("a.txt", "one\n"), ("c.txt", "three\n")],
        &[&root],
    );
    assert!(matches!(
        merge_onto(&git, &trunk, &related),
        Ok(Merge::Clean(_))
    ));

    match merge_onto(&git, &trunk, &orphan) {
        Err(_) => {}
        Ok(Merge::Clean(tree)) => panic!("an unrelated draft merged clean into {tree}"),
        Ok(Merge::Conflict(paths, _)) => panic!("an unrelated draft read as a conflict: {paths:?}"),
    }
}

#[test]
fn a_merge_whose_objects_are_missing_is_a_fault_never_a_clean_merge() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let root = commit(&test_box, &repo, &[("a.txt", "one\n")], &[]);
    let trunk = commit(&test_box, &repo, &[("a.txt", "two\n")], &[&root]);
    let git = Git::new(&repo);
    // A draft id that names no object here. Git exits 1, its conflict code, with no tree: bay
    // reads that as the fault it is, never as a conflict with no paths.
    let absent = "1".repeat(trunk.len());
    match merge_onto(&git, &trunk, &absent) {
        Err(_) => {}
        Ok(Merge::Clean(tree)) => panic!("a missing draft merged clean into {tree}"),
        Ok(Merge::Conflict(paths, _)) => panic!("a missing draft read as a conflict: {paths:?}"),
    }
}

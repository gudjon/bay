//! G669 (2026-11-06: a merge run took 65 s with a 1 s check, 30 s of it in "publish", and nothing
//! said which Git calls made that up). A runner's line for an operation ends with the Git commands
//! that took longest, each with its total time and how many times it ran.
//!
//! Guards to mutate: `src/git.rs` the recording around a Git call (no `git:` part); `take_times`, the
//! reset (a later operation's line also counts the one before it).
//! Not guarded: the reset in `src/executor.rs` before an operation, which keeps Git calls made between
//! operations out of its line. Removing it changes only what a line counts, and no test here sees it.

#![cfg(unix)]

use crate::support;

use support::{TestBox, unchecked_files};

/// The `git:` part of the runner's line about `id`.
fn git_part<'a>(stderr: &'a str, id: &str) -> &'a str {
    let line = stderr
        .lines()
        .find(|line| line.contains(id) && line.contains("merged"))
        .unwrap_or_else(|| panic!("no merged line for {id}: {stderr}"));
    line.split_once("git: ")
        .unwrap_or_else(|| panic!("no git part: {line}"))
        .1
}

fn count_of(part: &str, command: &str) -> u32 {
    part.split(", ")
        .find(|entry| entry.starts_with(&format!("{command} ")))
        .and_then(|entry| entry.rsplit_once(" x"))
        .map_or(0, |(_, count)| count.trim_end_matches(')').parse().unwrap())
}

#[test]
fn a_merge_line_names_the_git_commands_that_took_longest() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let start = |intent: &str| {
        let started = test_box.bay_ok(&repo, &["start", intent, "--paths", "src/"]);
        let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(format!("src/{intent}.rs")), "fn b() {}\n").unwrap();
        let queued = test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge")
            .output()
            .unwrap();
        assert!(queued.status.success(), "{queued:?}");
        started["change"].as_str().unwrap().to_owned()
    };
    let (one, two) = (start("one"), start("two"));
    let runner = test_box.clone_of(&remote, "runner");

    let out = test_box
        .bay(&runner)
        .args(["runner", "run", "--once"])
        .output()
        .unwrap();

    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let (first, second) = (git_part(&stderr, &one), git_part(&stderr, &two));
    assert!(!first.is_empty(), "{stderr}");
    // Each line counts its own operation: the second is not the first's total and more.
    let total = |part: &str| -> u32 {
        part.split(", ")
            .filter_map(|entry| entry.rsplit_once(" x"))
            .map(|(_, count)| count.trim_end_matches(')').parse::<u32>().unwrap())
            .sum()
    };
    assert!(total(second) < total(first) * 2, "{first} then {second}");
    assert!(
        count_of(first, "push") + count_of(second, "push") >= 2,
        "{stderr}"
    );
}

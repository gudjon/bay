//! G671 (a move of 4,154 files, 34 attempts over a day and a half: most of each 25-minute attempt
//! was one `git diff` per file per commit, 270,584 in one run). The search for merged work a change
//! would undo asks Git once per commit, and looks closer only at a file that could be undone. A file
//! another change created and this one moved was refused as undone though nothing was lost; it now
//! merges. A refusal names everything it found, so an agent fixes them in one round, not one per run.
//!
//! Guards to mutate, in `src/publish.rs`:
//! - `undone_by`, the `(status == "A") == merged.contains(path)` skip (one diff per moved file again).
//! - `moved_away` returning false (a moved file is refused).
//! - `moved_away` returning true (a file dropped, not moved, merges and the work is lost).
//! - `undoes_merged`, keeping only the first of `found` (the second file waits for another run).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, UNCHECKED_POLICY, result_json};

const TEXT: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";

fn seeded(test_box: &TestBox, files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        files,
        &[(".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned())],
    );
    let remote = test_box.path().join("repo-remote.git");
    let other = test_box.clone_of(&remote, "other");
    (repo, other)
}

fn saves(test_box: &TestBox, clone: &Path, file: &str, text: &str) -> String {
    test_box.git_ok(clone, &["pull", "-q", "--ff-only"]);
    let path = clone.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    let (code, saved) = result_json(test_box.bay(clone).args(["save", "theirs"]));
    assert_eq!(code, 0, "{saved}");
    saved["change"].as_str().unwrap().to_owned()
}

fn opened(test_box: &TestBox, repo: &Path) -> (String, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", "move", "--paths", "src/"]);
    (
        started["change"].as_str().unwrap().to_owned(),
        PathBuf::from(started["detail"]["view"].as_str().unwrap()),
    )
}

#[test]
fn a_file_another_change_created_can_be_moved_and_edited() {
    let test_box = TestBox::new();
    let (repo, other) = seeded(&test_box, &[("src/a.rs", TEXT)]);
    let (id, view) = opened(&test_box, &repo);
    saves(&test_box, &other, "src/new.rs", TEXT);
    test_box.bay_ok(&repo, &["start", &id]);
    std::fs::create_dir_all(view.join("src/moved")).unwrap();
    std::fs::remove_file(view.join("src/new.rs")).unwrap();
    std::fs::write(view.join("src/moved/new.rs"), TEXT.replace("two", "TWO")).unwrap();

    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
}

#[test]
fn a_file_another_change_created_and_this_one_dropped_is_refused() {
    let test_box = TestBox::new();
    let (repo, other) = seeded(&test_box, &[("src/a.rs", TEXT)]);
    let (id, view) = opened(&test_box, &repo);
    let theirs = saves(&test_box, &other, "src/new.rs", TEXT);
    test_box.bay_ok(&repo, &["start", &id]);
    std::fs::remove_file(view.join("src/new.rs")).unwrap();
    std::fs::create_dir_all(view.join("src/moved")).unwrap();
    std::fs::write(view.join("src/moved/other.rs"), "unrelated\n").unwrap();

    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "UNDOES_MERGED", "{refused}");
    assert_eq!(refused["detail"]["change"], theirs.as_str(), "{refused}");
}

#[test]
fn a_refusal_names_every_file_of_every_change_at_once() {
    let test_box = TestBox::new();
    let (repo, other) = seeded(
        &test_box,
        &[("src/a.rs", TEXT), ("src/b.rs", TEXT), ("src/c.rs", TEXT)],
    );
    let (id, view) = opened(&test_box, &repo);
    saves(&test_box, &other, "src/a.rs", &TEXT.replace("two", "TWO"));
    test_box.git_ok(&other, &["pull", "-q", "--ff-only"]);
    std::fs::write(other.join("src/b.rs"), TEXT.replace("two", "TWO")).unwrap();
    std::fs::write(other.join("src/c.rs"), TEXT.replace("two", "TWO")).unwrap();
    let (code, saved) = result_json(test_box.bay(&other).args(["save", "two files"]));
    assert_eq!(code, 0, "{saved}");
    test_box.bay_ok(&repo, &["start", &id]);
    for file in ["a", "b", "c"] {
        std::fs::write(view.join(format!("src/{file}.rs")), format!("{TEXT}more\n")).unwrap();
    }

    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    assert_eq!(code, 10, "{refused}");
    let detail = &refused["detail"];
    let named: Vec<&str> = detail["paths"]
        .as_array()
        .unwrap()
        .iter()
        .chain(detail["also"][0]["paths"].as_array().unwrap())
        .map(|path| path.as_str().unwrap())
        .collect();
    assert_eq!(named, ["src/b.rs", "src/c.rs", "src/a.rs"], "{refused}");
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let undoes = &status["detail"]["changes"][0]["operation"]["undoes"];
    assert_eq!(undoes["also"][0]["paths"][0], "src/a.rs", "{status}");
}

#[test]
fn moving_many_files_costs_a_git_diff_per_commit_at_most() {
    let test_box = TestBox::new();
    let files: Vec<(String, String)> = (0..60)
        .map(|n| (format!("src/f{n}.rs"), format!("{n}\n{TEXT}")))
        .collect();
    let seed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let (repo, other) = seeded(&test_box, &seed);
    let (_, view) = opened(&test_box, &repo);
    // One merged change edits 40 of the files this change moves: none of them can be undone.
    for n in 1..40 {
        let text = format!("{n}\n{TEXT}edited\n");
        std::fs::write(other.join(format!("src/f{n}.rs")), text).unwrap();
    }
    saves(
        &test_box,
        &other,
        "src/f0.rs",
        &format!("0\n{TEXT}edited\n"),
    );
    std::fs::create_dir_all(view.join("src/moved")).unwrap();
    for (path, text) in &files {
        let name = Path::new(path).file_name().unwrap();
        std::fs::remove_file(view.join(path)).unwrap();
        std::fs::write(view.join("src/moved").join(name), text).unwrap();
    }
    let queued = test_box
        .bay(&view)
        .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
        .arg("merge")
        .output()
        .unwrap();
    assert!(queued.status.success(), "{queued:?}");

    let remote = test_box.path().join("repo-remote.git");
    let runner = test_box.clone_of(&remote, "runner");
    let run = test_box
        .bay(&runner)
        .args(["runner", "run", "--once"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&run.stderr);
    let line = stderr
        .lines()
        .find(|line| line.contains("merged") && line.contains("git: "))
        .unwrap_or_else(|| panic!("no merged line: {stderr}"));
    let diffs = line
        .split_once("git: ")
        .unwrap()
        .1
        .split(", ")
        .find(|entry| entry.starts_with("diff "))
        .map_or(0, |entry| {
            let (_, tail) = entry.rsplit_once(" x").unwrap();
            tail.split_whitespace().next().unwrap().parse().unwrap()
        });
    assert!(diffs < 30, "one diff per file per commit: {line}");
}

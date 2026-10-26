//! G396 (a fleet finding, 2026-10-26: a check whose `paths` kept part of the tree compared the
//! tree it got with the request's `candidate.tree`, which can never match, and every head
//! failed for a quarter of an hour). A check whose tree is part of the candidate's is told the
//! Git tree of exactly that part, `candidate.paths_tree`, which the files it got hash to; a
//! check that gets the whole tree is told none.
//!
//! Guard to mutate: `src/check.rs` `request`: drop `request["candidate"]["paths_tree"]`.

// Each case runs a check adapter (a shell script), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

const FILES: &[(&str, &str)] = &[
    ("src/one.txt", "one\n"),
    ("src/deep/two.txt", "two\n"),
    ("docs/guide.md", "guide\n"),
    ("docs/faq.md", "faq\n"),
    ("assets/big/a.bin", "a\n"),
];

/// A trunk whose one check `tests` (its `[[check]]` table ends with `extra`) logs, on each run,
/// the Git tree its own files hash to and the request, and passes.
fn trunk(test_box: &TestBox, extra: &str) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let log = test_box.path().join("seen.log");
    let relay = test_box.relay(&format!("printf '%s\\n' \"$req\" >> '{}'", log.display()));
    let before = String::new()
        + "scratch=$(mktemp -d)\n"
        + "hash=$(GIT_DIR=\"$scratch\" GIT_INDEX_FILE=\"$scratch/index\" sh -c 'git init -q --bare \"$GIT_DIR\" && git --work-tree=. add -A . && git write-tree')\n"
        + "rm -rf \"$scratch\"\n"
        + "req=\"$hash|$(printf '%s' \"$req\" | tr -d '\\n')\"\n"
        + &relay;
    let files: Vec<(String, String)> = check_files(&before, &passing_result(1), 1, 60)
        .into_iter()
        .map(|(path, text)| {
            if path == ".bay/policy.toml" {
                (path, format!("{text}{extra}"))
            } else {
                (path, text)
            }
        })
        .collect();
    test_box.seed_with_check(&repo, FILES, &files);
    (repo, log)
}

/// The (hash, request) of the check's one run.
fn run(log: &Path) -> (String, Value) {
    let text = std::fs::read_to_string(log).unwrap();
    let line = text.lines().next().expect("the check never ran");
    let (hash, request) = line.split_once('|').unwrap();
    (hash.to_owned(), serde_json::from_str(request).unwrap())
}

fn land(test_box: &TestBox, repo: &Path, intent: &str) {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/one.txt"), "one, changed\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
}

#[test]
fn a_check_on_part_of_the_tree_is_told_that_parts_tree() {
    let test_box = TestBox::new();
    let (repo, log) = trunk(&test_box, "paths = [\"src/\", \"docs/guide.md\"]\n");
    land(&test_box, &repo, "PART-1");
    let (hash, request) = run(&log);
    assert_eq!(
        request["candidate"]["paths_tree"],
        hash.as_str(),
        "{request}"
    );
    assert_ne!(request["candidate"]["tree"], hash.as_str(), "{request}");
}

#[test]
fn a_check_on_the_whole_tree_is_told_no_parts_tree() {
    let test_box = TestBox::new();
    let (repo, log) = trunk(&test_box, "");
    land(&test_box, &repo, "PART-2");
    let (_, request) = run(&log);
    assert!(
        request["candidate"].get("paths_tree").is_none(),
        "{request}"
    );
}

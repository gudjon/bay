//! G74. `bay evidence missing --kind <k> --producer <p> [--since <rev>]` names the trunk
//! commits that hold no record of that kind from that producer.
//!
//! The population is trunk's first-parent line after the rev, and the count of commits off it
//! is stated. A record covers a landing only when its `subject.commit` is that commit, or its
//! `subject.base_tree` and `subject.tree` are the landing's first parent's tree and its tree:
//! a record is about a diff, and a tree alone is not one. So a landing that restores an earlier
//! tree is not covered by the record of the landing that first made that tree. A record from
//! another producer, or of another kind, covers nothing. Without a registry every row says
//! `trust: "unsigned"`.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::{Value, json};
use support::{TestBox, unchecked_files};

/// Writes `file` with `text` in `dir`, commits it and returns the commit.
fn commit(test_box: &TestBox, dir: &Path, file: &str, text: &str) -> String {
    std::fs::write(dir.join(file), text).unwrap();
    test_box.git_ok(dir, &["add", file]);
    test_box.git_ok(dir, &["commit", "-q", "-m", &format!("{file}: {text}")]);
    test_box.git_ok(dir, &["rev-parse", "HEAD"])
}

fn tree_of(test_box: &TestBox, dir: &Path, rev: &str) -> String {
    test_box.git_ok(dir, &["rev-parse", &format!("{rev}^{{tree}}")])
}

/// A `bay.evidence/1` record from `producer` of `kind`, about `commit` (tree on base tree).
fn record(producer: &str, n: u32, kind: &str, tree: &str, commit: &str, base_tree: &str) -> Value {
    json!({
        "schema": "bay.evidence/1",
        "id": format!("{producer}/{n}"),
        "kind": kind,
        "subject": {"tree": tree, "commit": commit, "base_tree": base_tree},
        "check": {"id": kind, "environment": "local"},
        "policy_revision": 1,
        "manifest_digest": "sha256:00",
        "producer": {"name": producer, "version": "1"},
        "verdict": "pass",
        "at": "2026-10-19T00:00:00Z",
    })
}

/// Writes `records` as `producer`'s note on `tree`.
fn note(test_box: &TestBox, dir: &Path, producer: &str, tree: &str, records: &[Value]) {
    let text = serde_json::to_string(records).unwrap();
    test_box.git_ok(
        dir,
        &[
            "notes",
            &format!("--ref=refs/notes/bay/evidence/{producer}"),
            "add",
            "-f",
            "-m",
            &text,
            tree,
        ],
    );
}

fn missing(test_box: &TestBox, dir: &Path, since: Option<&str>) -> Value {
    let mut args = vec![
        "evidence",
        "missing",
        "--kind",
        "review",
        "--producer",
        "reviewer",
    ];
    if let Some(since) = since {
        args.extend(["--since", since]);
    }
    let result = test_box.bay_ok(dir, &args);
    assert_eq!(result["decision"], "ok", "{result}");
    result["detail"].clone()
}

fn commits(detail: &Value) -> Vec<String> {
    detail["missing"]
        .as_array()
        .unwrap_or_else(|| panic!("no missing list: {detail}"))
        .iter()
        .map(|row| row["commit"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn only_a_record_of_the_kind_from_the_producer_for_the_commit_covers_a_landing() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let seed = test_box.seed_with_check(&repo, &[("x", "0\n")], &unchecked_files());

    let first = commit(&test_box, &repo, "x", "1\n");
    let second = commit(&test_box, &repo, "x", "2\n");
    // Restores the tree `first` made.
    let restored = commit(&test_box, &repo, "x", "1\n");
    let other_producer = commit(&test_box, &repo, "y", "d\n");
    let other_kind = commit(&test_box, &repo, "z", "e\n");
    test_box.git_ok(&repo, &["switch", "-q", "-c", "side"]);
    let side = commit(&test_box, &repo, "s", "side\n");
    test_box.git_ok(&repo, &["switch", "-q", "main"]);
    test_box.git_ok(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    let merge = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);

    let tree = |rev: &str| tree_of(&test_box, &repo, rev);
    assert_eq!(tree(&restored), tree(&first));
    // A review of `first`, by commit.
    note(
        &test_box,
        &repo,
        "reviewer",
        &tree(&first),
        &[record(
            "reviewer",
            1,
            "review",
            &tree(&first),
            &first,
            &tree(&seed),
        )],
    );
    // A review of `second` by its diff (base tree and tree), naming no commit bay knows.
    let unknown_commit = "0".repeat(first.len());
    note(
        &test_box,
        &repo,
        "reviewer",
        &tree(&second),
        &[record(
            "reviewer",
            2,
            "review",
            &tree(&second),
            &unknown_commit,
            &tree(&first),
        )],
    );
    // A review by another producer.
    note(
        &test_box,
        &repo,
        "another",
        &tree(&other_producer),
        &[record(
            "another",
            1,
            "review",
            &tree(&other_producer),
            &other_producer,
            &tree(&restored),
        )],
    );
    // A record of another kind from the producer.
    note(
        &test_box,
        &repo,
        "reviewer",
        &tree(&other_kind),
        &[record(
            "reviewer",
            3,
            "check",
            &tree(&other_kind),
            &other_kind,
            &tree(&other_producer),
        )],
    );
    test_box.git_ok(
        &repo,
        &["push", "-q", "origin", "refs/notes/bay/evidence/*"],
    );

    // A reader that fetches everything from the remote.
    let reader = test_box.clone_of(&remote, "reader");
    let detail = missing(&test_box, &reader, Some(&seed));
    assert_eq!(
        commits(&detail),
        [
            restored.clone(),
            other_producer.clone(),
            other_kind.clone(),
            merge.clone()
        ],
        "the restored tree, another producer and another kind cover nothing, and the side \
         commit {side} is off the first-parent line: {detail}"
    );
    assert_eq!(detail["off_first_parent"], 1, "{detail}");
    assert_eq!(detail["trust"], "unsigned", "{detail}");
    let rows = detail["missing"].as_array().unwrap();
    let row = &rows[0];
    assert_eq!(row["tree"], tree(&restored).as_str(), "{row}");
    assert_eq!(row["base"], second.as_str(), "{row}");
    assert_eq!(row["trust"], "unsigned", "{row}");
    assert_eq!(rows[3]["base"], other_kind.as_str(), "{detail}");

    // With no --since, all of trunk's first-parent line, the seed first.
    let all = missing(&test_box, &reader, None);
    assert_eq!(
        commits(&all),
        [seed, restored, other_producer, other_kind, merge],
        "{all}"
    );
}

#[test]
fn a_rev_that_names_no_commit_fails_and_says_so() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("x", "0\n")], &unchecked_files());
    let (code, result) = support::result_json(test_box.bay(&repo).args([
        "evidence",
        "missing",
        "--kind",
        "review",
        "--producer",
        "reviewer",
        "--since",
        "no-such-rev",
    ]));
    assert_ne!(code, 0, "{result}");
    assert!(
        result["detail"]
            .as_str()
            .is_some_and(|text| text.contains("no-such-rev")),
        "{result}"
    );
}

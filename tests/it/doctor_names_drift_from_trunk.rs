//! G157. Doctor's `repository` row names drift from trunk.
//!
//! A checkout that runs code no commit describes, or one far behind trunk, is named: the row
//! gives HEAD's distance from the trunk tracking ref (ahead and behind, from local refs only,
//! never a fetch) and the count of tracked files that differ from HEAD. It is informational:
//! green, with a note when HEAD is behind or files are modified.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::{TestBox, result_json, row};

fn repository(test_box: &TestBox, repo: &std::path::Path) -> Value {
    let (code, result) = result_json(test_box.bay(repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    row(&result, "repository").clone()
}

#[test]
fn the_repository_row_names_distance_from_trunk_and_modified_files() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")],
    );

    let clean = repository(&test_box, &repo);
    assert_eq!(clean["state"], "green", "{clean}");
    assert_eq!(clean["trunk"], "refs/remotes/origin/main", "{clean}");
    assert_eq!(clean["ahead"], 0, "{clean}");
    assert_eq!(clean["behind"], 0, "{clean}");
    assert_eq!(clean["modified"], 0, "{clean}");
    assert!(clean.get("note").is_none(), "{clean}");

    // Two tracked files differ from HEAD; an untracked file is not a modified tracked file.
    std::fs::write(repo.join("src/a.rs"), "fn a() { 1; }\n").unwrap();
    std::fs::write(repo.join("src/b.rs"), "fn b() { 2; }\n").unwrap();
    std::fs::write(repo.join("untracked.txt"), "new\n").unwrap();
    let modified = repository(&test_box, &repo);
    assert_eq!(modified["state"], "green", "{modified}");
    assert_eq!(modified["modified"], 2, "{modified}");
    assert!(
        modified["note"]
            .as_str()
            .is_some_and(|note| note.contains("2 tracked files")),
        "{modified}"
    );
    test_box.git_ok(&repo, &["checkout", "-q", "--", "src/"]);

    // A local commit is ahead.
    std::fs::write(repo.join("src/c.rs"), "fn c() {}\n").unwrap();
    test_box.git_ok(&repo, &["add", "src/c.rs"]);
    test_box.git_ok(&repo, &["commit", "-q", "-m", "local"]);
    let ahead = repository(&test_box, &repo);
    assert_eq!(ahead["ahead"], 1, "{ahead}");
    assert_eq!(ahead["behind"], 0, "{ahead}");

    // Trunk moves on the remote: doctor does not fetch, so it is not behind until a fetch.
    let bare = std::path::PathBuf::from(test_box.git_ok(&repo, &["remote", "get-url", "origin"]));
    let other = test_box.clone_of(&bare, "other");
    for file in ["one", "two", "three"] {
        std::fs::write(other.join(file), "moved\n").unwrap();
        test_box.git_ok(&other, &["add", file]);
        test_box.git_ok(&other, &["commit", "-q", "-m", file]);
    }
    test_box.git_ok(&other, &["push", "-q", "origin", "main"]);
    let unfetched = repository(&test_box, &repo);
    assert_eq!(unfetched["behind"], 0, "{unfetched}");
    test_box.git_ok(&repo, &["fetch", "-q", "origin"]);
    let behind = repository(&test_box, &repo);
    assert_eq!(behind["state"], "green", "{behind}");
    assert_eq!(behind["ahead"], 1, "{behind}");
    assert_eq!(behind["behind"], 3, "{behind}");
    assert!(
        behind["note"]
            .as_str()
            .is_some_and(|note| note.contains("3 commits behind")),
        "{behind}"
    );
}

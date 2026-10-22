//! G73. `bay status --since --merged --since <cursor>` lists the landings after the cursor, and only
//! them.
//!
//! The stream is trunk's first-parent line after the cursor's trunk, oldest first: each
//! landing names its change (or `null`), its base (the first parent), the commit, the actor
//! and the committer time. A commit landed outside bay stays in the stream, marked
//! `outside_bay: true`, never dropped. A commit reached only through a merge's other parent is
//! not a landing; the stream counts those as `off_first_parent`, so its population is stated.
//! A watch from the cursor it returns lists nothing new.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

fn landed(test_box: &TestBox, dir: &Path, since: Option<&str>) -> Value {
    let mut args = vec!["status", "--since"];
    if let Some(cursor) = since {
        args.push(cursor);
    }
    args.push("--merged");
    let watched = test_box.bay_ok(dir, &args);
    assert_eq!(watched["decision"], "ok", "{watched}");
    assert!(watched["detail"]["cursor"].is_string(), "{watched}");
    watched["detail"].clone()
}

/// Commits `file` in `dir` at committer time `date` and returns the commit.
fn commit(test_box: &TestBox, dir: &Path, file: &str, date: &str) -> String {
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
    test_box.git_ok(dir, &["rev-parse", "HEAD"])
}

/// The committer time of `rev`, in UTC, in bay's form.
fn committer_time(test_box: &TestBox, dir: &Path, rev: &str) -> String {
    let out = test_box
        .git(dir)
        .env("TZ", "UTC")
        .args([
            "show",
            "-s",
            "--date=format-local:%Y-%m-%dT%H:%M:%SZ",
            "--format=%cd",
            rev,
        ])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn landings_after_a_cursor_name_base_time_and_landings_outside_bay() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let trunk_at =
        test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());

    // Outside a view, a watch with no cursor lists nothing and returns the current trunk.
    let first = landed(&test_box, &repo, None);
    assert_eq!(first["landings"], serde_json::json!([]), "{first}");
    assert_eq!(first["off_first_parent"], 0, "{first}");
    let cursor = first["cursor"].as_str().unwrap().to_owned();

    // A change lands through bay.
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "merged", "{result}");
    test_box.git_ok(&repo, &["pull", "-q", "--ff-only", "origin", "main"]);
    let published = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);

    // A commit pushed straight to trunk, outside bay.
    let outside = commit(&test_box, &repo, "outside.txt", "2026-01-02T03:04:05Z");
    // A merge of a side branch: its second parent is no landing.
    test_box.git_ok(&repo, &["switch", "-q", "-c", "side"]);
    let side = commit(&test_box, &repo, "side.txt", "2026-01-03T00:00:00Z");
    test_box.git_ok(&repo, &["switch", "-q", "main"]);
    let out = test_box
        .git(&repo)
        .env("GIT_COMMITTER_DATE", "2026-01-04T00:00:00Z")
        .args(["merge", "-q", "--no-ff", "-m", "merge side", "side"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let merge = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);

    let seen = landed(&test_box, &repo, Some(&cursor));
    let landings = seen["landings"].as_array().unwrap();
    let commits: Vec<&str> = landings
        .iter()
        .map(|landing| landing["commit"].as_str().unwrap())
        .collect();
    assert_eq!(
        commits,
        [published.as_str(), outside.as_str(), merge.as_str()],
        "the first-parent line, oldest first, and never the side commit {side}: {seen}"
    );
    assert_eq!(seen["off_first_parent"], 1, "{seen}");
    assert_eq!(seen["more"], false, "{seen}");
    // A landed stream carries landings only.
    assert!(seen.get("events").is_none(), "{seen}");

    let bay = &landings[0];
    assert_eq!(bay["change"], id.as_str(), "{bay}");
    assert_eq!(bay["outside_bay"], false, "{bay}");
    assert_eq!(bay["base"], trunk_at.as_str(), "{bay}");
    assert!(bay["actor"].is_string(), "{bay}");
    assert_eq!(
        bay["at"],
        committer_time(&test_box, &repo, &published).as_str(),
        "{bay}"
    );

    let direct = &landings[1];
    assert_eq!(direct["change"], Value::Null, "{direct}");
    assert_eq!(direct["outside_bay"], true, "{direct}");
    assert_eq!(direct["base"], published.as_str(), "{direct}");
    assert_eq!(direct["at"], "2026-01-02T03:04:05Z", "{direct}");

    let merged = &landings[2];
    assert_eq!(merged["outside_bay"], true, "{merged}");
    assert_eq!(
        merged["base"],
        outside.as_str(),
        "the first parent: {merged}"
    );
    assert_eq!(merged["at"], "2026-01-04T00:00:00Z", "{merged}");

    // Nothing new after the cursor it returned.
    let next = seen["cursor"].as_str().unwrap();
    let again = landed(&test_box, &repo, Some(next));
    assert_eq!(again["landings"], serde_json::json!([]), "{again}");
    assert_eq!(again["off_first_parent"], 0, "{again}");
}

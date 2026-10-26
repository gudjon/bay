//! G97: a view of an ended change is released, and a view that holds newer or uncheckpointed work
//! never is (`DESIGN` § 10; findings from a pilot that removed a landed view).
//!
//! The next `bay start` on the machine removes each view whose change landed, was abandoned or
//! ended `no_effect`, at once (owner ruling 2026-10-26: `[retention] window_s` keeps the chain's
//! records, never a view; a fleet's shared checkout held 574 views), checking right before
//! that the view holds nothing newer than the revision the change ended with. The view's worktree
//! and its journal both go, so `bay status` no longer lists it. A view with an edit, an untracked
//! file or a commit of its own, a locked worktree, a directory that is no worktree, the view the
//! caller stands in and the view of a change that has not ended are all kept.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use std::path::{Path, PathBuf};

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

fn listed(test_box: &TestBox, repo: &Path, id: &str) -> bool {
    let status = test_box.bay_ok(repo, &["status"]);
    status["detail"]["leftovers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["change"] == id)
}

/// A trunk whose one check fails a candidate holding `src/refused.rs`, with `[retention]
/// window_s` set to `window_s`.
fn seeded(test_box: &TestBox, window_s: u64) -> PathBuf {
    let repo = test_box.repo_any("repo");
    let pass = passing_result(1);
    let fail = pass.replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let result = format!("if [ -e src/refused.rs ]; then {fail}; else {pass}; fi");
    let mut files = check_files("true", &result, 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        format!("[retention]\nwindow_s = {window_s}\n"),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    repo
}

/// Starts change `name` on `src/` and writes `src/<name>.rs` in its view. Returns (id, view).
fn start_with(test_box: &TestBox, repo: &Path, name: &str) -> (String, PathBuf) {
    let intent = format!("CARD-{}", name.to_uppercase());
    let started = test_box.bay_ok(repo, &["start", &intent, "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(
        view.join(format!("src/{name}.rs")),
        format!("fn {name}() {{}}\n"),
    )
    .unwrap();
    (id, view)
}

fn land(test_box: &TestBox, view: &Path) {
    let (code, landed) = result_json(test_box.bay(view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
}

/// The journal of change `id`: `<git-common-dir>/bay/views/<id>.journal`.
fn journal(test_box: &TestBox, repo: &Path, id: &str) -> PathBuf {
    let common = test_box.git_ok(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    );
    PathBuf::from(common).join(format!("bay/views/{id}.journal"))
}

#[test]
fn a_landed_view_is_released_and_every_view_with_work_or_in_use_is_kept() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, 0);

    // Every change starts before any lands: each `start` releases what has ended by then.
    let (clean, clean_view) = start_with(&test_box, &repo, "clean");
    let (tracked, tracked_view) = start_with(&test_box, &repo, "tracked");
    let (untracked, untracked_view) = start_with(&test_box, &repo, "untracked");
    let (committed, committed_view) = start_with(&test_box, &repo, "committed");
    let (locked, locked_view) = start_with(&test_box, &repo, "locked");
    let (standing, standing_view) = start_with(&test_box, &repo, "standing");
    let (damaged, damaged_view) = start_with(&test_box, &repo, "damaged");
    let (refused, refused_view) = start_with(&test_box, &repo, "refused");
    for view in [
        &clean_view,
        &tracked_view,
        &untracked_view,
        &committed_view,
        &locked_view,
        &standing_view,
        &damaged_view,
    ] {
        land(&test_box, view);
    }
    // A change with an operation that has not ended: its check refused it.
    let (code, blocked) = result_json(test_box.bay(&refused_view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{blocked}");

    // After the landings: an edit of a tracked file, a new untracked file, a commit made in the
    // view whose tree is the landed revision's, a locked worktree, a view with no `.git`.
    std::fs::write(tracked_view.join("src/a.rs"), "fn a() { edited() }\n").unwrap();
    std::fs::write(untracked_view.join("src/more.rs"), "fn more() {}\n").unwrap();
    test_box.git_ok(&committed_view, &["add", "--all", "--sparse"]);
    test_box.git_ok(
        &committed_view,
        &["commit", "-q", "--allow-empty", "-m", "more"],
    );
    let locked_text = locked_view.display().to_string();
    test_box.git_ok(&repo, &["worktree", "lock", &locked_text]);
    std::fs::remove_file(damaged_view.join(".git")).unwrap();
    for id in [&clean, &tracked, &untracked, &committed, &locked, &standing] {
        assert!(listed(&test_box, &repo, id), "{id} is listed before");
    }

    // The next start runs from inside the standing view.
    let started = test_box.bay_ok(&standing_view, &["start", "CARD-NEXT", "--paths", "src/"]);
    let released = started["detail"]["released_views"].clone();
    assert_eq!(released, serde_json::json!([clean]), "{started}");
    assert!(!clean_view.exists(), "the landed view is gone");
    assert!(
        !journal(&test_box, &repo, &clean).exists(),
        "its journal is gone"
    );
    assert!(!listed(&test_box, &repo, &clean), "status forgets it");

    let kept = [
        (
            &tracked,
            &tracked_view,
            "src/a.rs",
            "an edited tracked file",
        ),
        (
            &untracked,
            &untracked_view,
            "src/more.rs",
            "an untracked file",
        ),
        (
            &committed,
            &committed_view,
            "src/committed.rs",
            "a commit of its own",
        ),
        (&locked, &locked_view, "src/locked.rs", "a locked worktree"),
        (
            &standing,
            &standing_view,
            "src/standing.rs",
            "the view start ran in",
        ),
        (
            &damaged,
            &damaged_view,
            "src/damaged.rs",
            "a view with no .git",
        ),
        (
            &refused,
            &refused_view,
            "src/refused.rs",
            "a change not ended",
        ),
    ];
    for (id, view, file, why) in kept {
        assert!(view.join(file).exists(), "{why}: the view is kept");
        assert!(
            journal(&test_box, &repo, id).exists(),
            "{why}: the journal is kept"
        );
    }
    assert!(listed(&test_box, &repo, &untracked) && listed(&test_box, &repo, &refused));
}

#[test]
fn a_landed_view_is_released_without_waiting_for_the_retention_window() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box, 86_400);
    let (clean, clean_view) = start_with(&test_box, &repo, "clean");
    land(&test_box, &clean_view);

    let started = test_box.bay_ok(&repo, &["start", "CARD-NEXT", "--paths", "src/"]);
    assert_eq!(
        started["detail"]["released_views"],
        serde_json::json!([clean]),
        "{started}"
    );
    assert!(!clean_view.exists(), "the landed view is released");
    assert!(!journal(&test_box, &repo, &clean).exists());
    assert!(!listed(&test_box, &repo, &clean));
}

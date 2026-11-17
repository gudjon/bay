//! G769 (owner, 2026-11-16: "greatly optimizing for disk space for users"; a Mac held 5.2 GB in the
//! views of 15 merged changes, each clean at an older draft bay itself wrote, so each read as
//! holding newer work and stayed). A view of an ended change whose `HEAD` is a commit bay wrote,
//! with nothing else in it (no edit, no untracked file, no ignored file), goes at the next start,
//! and its `HEAD` stays in the store under `refs/bay/view-kept/<change>`. The same view with an
//! untracked file stays, and a commit an agent made keeps its view (G97).
//!
//! Guard to mutate: in `src/view.rs` `release`, drop the `committed_only` branch, and the view
//! stays.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

fn seeded(test_box: &TestBox) -> PathBuf {
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    repo
}

/// Starts, edits and merges change `name`; its view is then moved to a commit bay's own
/// identity wrote, with nothing else in it. Returns (id, view, that commit).
fn merged_at_bays_commit(test_box: &TestBox, repo: &Path, name: &str) -> (String, PathBuf, String) {
    let started = test_box.bay_ok(repo, &["start", &format!("CARD-{name}"), "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(
        view.join(format!("src/{name}.rs")),
        format!("fn {name}() {{}}\n"),
    )
    .unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    // bay's own identity, as bay's records carry it (`crate::change::RECORD_IDENTITY`).
    let status = std::process::Command::new("git")
        .args([
            "commit",
            "-q",
            "--allow-empty",
            "--no-gpg-sign",
            "-m",
            "an older draft",
        ])
        .current_dir(&view)
        .env("GIT_AUTHOR_NAME", "bay")
        .env("GIT_AUTHOR_EMAIL", "bay@bay.invalid")
        .env("GIT_COMMITTER_NAME", "bay")
        .env("GIT_COMMITTER_EMAIL", "bay@bay.invalid")
        .status()
        .unwrap();
    assert!(status.success());
    let head = test_box.git_ok(&view, &["rev-parse", "HEAD"]);
    (id, view, head.trim().to_owned())
}

#[test]
fn a_view_holding_only_bays_own_commit_goes_and_its_commit_stays() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let (freed, freed_view, freed_head) = merged_at_bays_commit(&test_box, &repo, "freed");
    let (kept, kept_view, _) = merged_at_bays_commit(&test_box, &repo, "kept");
    std::fs::write(kept_view.join("notes.txt"), "mine\n").unwrap();

    // WHEN the next change starts, which releases views of ended changes
    let _ = test_box.bay_ok(&repo, &["start", "CARD-NEXT", "--paths", "src/"]);

    // THEN the view holding only bay's commit is gone, and its commit is kept by a ref
    assert!(!freed_view.exists(), "the view of {freed} stayed");
    let pinned = test_box.git_ok(
        &repo,
        &["rev-parse", &format!("refs/bay/view-kept/{freed}")],
    );
    assert_eq!(pinned.trim(), freed_head);
    // AND the view with an untracked file stays
    assert!(
        kept_view.exists(),
        "the view of {kept} with a file of its own was removed"
    );
}

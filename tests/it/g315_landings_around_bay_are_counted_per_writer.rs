//! G315. Landings around bay are counted per writer.
//!
//! `bay status` lists the trunk commits that did not land through bay under
//! `detail.unattributed`. Its `by_writer` groups them by the committer Git records on each
//! commit, `Name <email>`, read in the same log as the landings, the most first, so a team sees
//! who still lands around bay. bay's own landing is counted under no writer.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/unattributed.rs` `scan`: `"%ct%x1f%ce%x1f%s"` to `"%ct%x1f%cn%x1f%s"` (each writer
//!   names its name twice, not its email).
//! - `src/unattributed.rs` `scan`: `.or_insert(0_u64) += 1;` to `+= 0;` (every count is 0).
//! - `src/unattributed.rs` `scan`: drop `"by_writer": by_writer(writers),` (no `by_writer`).
//! - `src/unattributed.rs` `by_writer`: `b.1.cmp(&a.1)` to `a.1.cmp(&b.1)` (the fewest first).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::json;
use support::{TestBox, result_json, unchecked_files};

/// Commits `file` in `dir` as committer `name <email>`.
fn commit_as(test_box: &TestBox, dir: &Path, file: &str, name: &str, email: &str) {
    std::fs::write(dir.join(file), format!("{file}\n")).unwrap();
    test_box.git_ok(dir, &["add", file]);
    let out = test_box
        .git(dir)
        .env("GIT_COMMITTER_NAME", name)
        .env("GIT_COMMITTER_EMAIL", email)
        .args(["commit", "-q", "-m", file])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn each_writer_who_landed_around_bay_is_counted() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());

    // A change lands through bay.
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    test_box.git_ok(&repo, &["pull", "-q", "--ff-only", "origin", "main"]);

    // Then two writers push straight to the trunk, around bay: one twice, one once.
    commit_as(
        &test_box,
        &repo,
        "one.txt",
        "Writer One",
        "one@example.invalid",
    );
    commit_as(
        &test_box,
        &repo,
        "two.txt",
        "Writer Two",
        "two@example.invalid",
    );
    commit_as(
        &test_box,
        &repo,
        "three.txt",
        "Writer One",
        "one@example.invalid",
    );
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);

    let status = test_box.bay_ok(&repo, &["status"]);
    let unattributed = &status["detail"]["unattributed"];
    assert_eq!(unattributed["count"], 3, "{status}");
    assert_eq!(unattributed["through_bay"], 1, "{status}");
    assert_eq!(
        unattributed["by_writer"],
        json!([
            {"writer": "Writer One <one@example.invalid>", "count": 2},
            {"writer": "Writer Two <two@example.invalid>", "count": 1},
        ]),
        "{status}"
    );
}

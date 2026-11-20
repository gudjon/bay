//! G824 (a fleet finding, 2026-11-20). A `bay merge --wait` answered that bay could not finish,
//! on a change a runner had merged: another session's `bay start` was removing the merged change's
//! view while the wait read it a last time, and the view's journal was still there. A release now
//! takes the view's lock first and leaves a view whose lock a merge holds. A merge takes the lock
//! before it reads the view, so a release under way ends first, and the record then answers.
//!
//! Guards to mutate: in `src/view.rs` `release`, the skip of a view whose lock is held (the start
//! removes the view); in `src/publish.rs` `submit_land`, the `view::hold` before the view is
//! read (the merge reads the view half removed and answers that bay could not finish).

// Each case runs shell scripts (the check's adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::fs::OpenOptions;
use std::process::Stdio;
use std::time::Duration;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_view_is_not_released_while_a_merge_reads_it() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    let landing = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let home = view.parent().unwrap().to_path_buf();
    let journal = home.join(format!("{id}.journal"));
    assert!(journal.exists(), "the view has no journal");

    // GIVEN a merge that reads the view: it holds the view's lock.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.join(format!("{id}.lock")))
        .unwrap();
    lock.lock().unwrap();

    // WHEN the next start releases the ended views, THEN it leaves this one.
    test_box.bay_ok(&repo, &["start", "CARD-2", "--paths", "src/"]);
    assert!(view.exists(), "a start removed a view a merge was reading");

    // GIVEN a release under way, holding the lock: the view's files are gone, its journal not yet.
    std::fs::remove_dir_all(&view).unwrap();
    // WHEN the change is merged again meanwhile
    let asked = test_box
        .bay_process(&repo)
        .args(["merge", id.as_str(), "--wait", "--json"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(3));
    // and the release ends: its journal goes, and then its lock.
    test_box.git_ok(&repo, &["worktree", "prune"]);
    std::fs::remove_file(&journal).unwrap();
    drop(lock);
    let out = asked.wait_with_output().unwrap();
    let again: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "not JSON ({error}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });

    // THEN the merge waited for the release, and the record answers: merged, with its commit.
    assert_eq!(out.status.code(), Some(0), "{again}");
    assert_eq!(again["decision"], "merged", "{again}");
    assert_eq!(again["detail"]["commit"], landing.as_str(), "{again}");
    let text = test_box.git_ok(
        &repo,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    let record: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        record["operations"].as_array().unwrap().len(),
        1,
        "no operation is written: {record}"
    );
}

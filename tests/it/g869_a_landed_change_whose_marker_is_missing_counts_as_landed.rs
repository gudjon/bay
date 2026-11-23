//! G869 (a fleet finding, 2026-11-23: the five later changes of one landing batch kept their
//! landed records and their queue releases, but the remote held no landed marker for any of
//! them, so a change that lands after one of them waited for ever).
//!
//! A change the remote holds no landed marker for, whose record says it landed, is on trunk at
//! its one publication the trunk scan finds: a `bay start --fixes` of it is accepted, and a
//! change that lands after it lands. The push that lands the change after it carries the
//! missing marker, create-only, at the same publication.
//!
//! The guards to mutate, each of which turns the case red:
//!
//! - `crate::undo::unmarked` in `src/undo.rs`: answer `Ok(None)`, and the fix is refused
//!   `CHANGE_NOT_LANDED`;
//! - `crate::undo::landed_commit`: return `Ok(None)` when the remote holds no marker, and the
//!   fix is refused `CHANGE_NOT_LANDED`;
//! - `unlanded` in `src/after.rs`, the `if let Some(trunk) = unmarked_on` block: drop it, and
//!   the dependent waits `DEPENDENCY_PENDING`;
//! - `Riding::unmarked` in `src/publish.rs`: make it push nothing, and the marker stays
//!   missing.

// Each case runs shell scripts (the check adapter), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// A bay-hosted remote (its hook judges every push) with an enrolled key registry.
fn hosted() -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    (test_box, host, repo)
}

fn start(test_box: &TestBox, repo: &Path, args: &[&str]) -> (i32, Value) {
    result_json(test_box.bay(repo).arg("start").args(args))
}

/// Starts a change on `intent` landing after each of `after`, and writes `file` in its view.
/// Returns (id, view).
fn start_writing(
    test_box: &TestBox,
    repo: &Path,
    intent: &str,
    file: &str,
    after: &[&str],
) -> (String, PathBuf) {
    let mut args = vec![intent, "--paths", "src/"];
    for dependency in after {
        args.extend(["--after", *dependency]);
    }
    let (code, started) = start(test_box, repo, &args);
    assert_eq!(code, 0, "{started}");
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), format!("// {file}\n")).unwrap();
    (started["change"].as_str().unwrap().to_owned(), view)
}

/// `bay merge --wait` in `view`.
fn land_wait(test_box: &TestBox, view: &Path) -> (i32, Value) {
    result_json(test_box.bay(view).args(["merge", "--wait"]))
}

#[test]
fn a_landed_change_whose_marker_is_missing_counts_as_landed_and_gets_its_marker_back() {
    let (test_box, host, repo) = hosted();
    let (first, first_view) = start_writing(&test_box, &repo, "CARD-8691", "src/first.rs", &[]);
    let (code, landed) = land_wait(&test_box, &first_view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let marker = format!("refs/bay/landed/{first}");
    let publication = test_box
        .git_bare(&host, &["rev-parse", &marker])
        .trim()
        .to_owned();

    // The remote loses the marker; the change's record still says it landed.
    test_box.git_bare(&host, &["update-ref", "-d", &marker]);
    let gone = test_box.git_bare(&host, &["for-each-ref", &marker]);
    assert!(gone.trim().is_empty(), "{gone}");

    // A fix of it is accepted: it is on trunk.
    let (code, fix) = start(
        &test_box,
        &repo,
        &["CARD-8692", "--paths", "src/", "--fixes", &first],
    );
    assert_eq!(code, 0, "{fix}");
    assert_ne!(fix["reason"], "CHANGE_NOT_LANDED", "{fix}");

    // A change after it lands after it, and its landing brings the marker back.
    let (second, second_view) = start_writing(
        &test_box,
        &repo,
        "CARD-8693",
        "src/second.rs",
        &[first.as_str()],
    );
    let (code, landed) = land_wait(&test_box, &second_view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let ids = test_box.trunk_change_ids(&host);
    assert_eq!(ids[..2], [second.as_str(), first.as_str()], "trunk order");
    let back = test_box.git_bare(&host, &["for-each-ref", "--format=%(objectname)", &marker]);
    assert_eq!(back.trim(), publication, "the marker of {first}");
}

//! G617 (a fleet finding, 2026-10-31: after G615 and G616 the one sweep of a `bay status` still
//! took 3.2 to 4.0 s of each read on a hosted remote, however few refs had moved). A listing's
//! sweep serves every status read of its store for a short window after it: such a read asks the
//! remote nothing and names that sweep's time as its `fetched_at`. Every sweep settles every live
//! chain (G619), so a read that names one change opens the window too; a read of a change the
//! store does not hold still asks the remote. How a push ends the window is G618. Read from the trace a test build writes when `BAY_TEST_GIT_TRACE`
//! names a file (G171); `BAY_TEST_SWEPT_S` sets the window in a test build.
//!
//! Guards to mutate: in `src/status.rs`, never serve a read from the window, and the second read
//! asks the remote; name the time of the read as a windowed read's `fetched_at`, and it is not
//! the sweep's; settle only the named chain of a named read's sweep, and the listing after it is
//! served chains it did not settle; serve a named read of a chain the store lacks, and the change is not shown.

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

/// A window no test outlives.
pub(crate) const OPEN: &str = "600";

/// A repository with one live change, and a reader clone that holds its chain: the box, the
/// first clone, the remote, the reader and the change id.
pub(crate) fn store() -> (TestBox, PathBuf, PathBuf, PathBuf, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let (id, _) = test_box.start_and_edit(&repo);
    let reader = test_box.clone_of(&remote, "reader");
    test_box.git_ok(
        &reader,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/changes/*:refs/bay/changes/*",
        ],
    );
    (test_box, repo, remote, reader, id)
}

/// `bay status [<id>]` in `dir` with a window of `window` seconds: the result, and how many of
/// bay's own git spawns asked the remote.
pub(crate) fn status(
    test_box: &TestBox,
    dir: &Path,
    id: Option<&str>,
    window: &str,
    name: &str,
) -> (usize, Value) {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let mut cmd = test_box.bay(dir);
    cmd.arg("status")
        .args(id)
        .env("BAY_ACTOR", "reader")
        .env("BAY_TEST_SWEPT_S", window)
        .env("BAY_TEST_GIT_TRACE", &trace)
        // What the read asks the remote is under test, never a saved answer of it (G714).
        .env("BAY_TEST_SAVED", "off");
    let (code, result) = result_json(&mut cmd);
    assert_eq!(code, 0, "{result}");
    let text = std::fs::read_to_string(&trace).expect("the trace was written");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let asked = lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref())
        .filter(|line| matches!(line["sub"].as_str(), Some("fetch" | "ls-remote" | "push")))
        .count();
    (asked, result)
}

/// Moves the remote's trunk one commit forward, as another clone's landing would: the new tip.
pub(crate) fn move_trunk(test_box: &TestBox, remote: &Path) -> String {
    move_ref(test_box, remote, "refs/heads/main")
}

/// Moves the remote's ref `name` one commit forward, its tree kept: the new tip.
pub(crate) fn move_ref(test_box: &TestBox, remote: &Path, name: &str) -> String {
    let main = test_box.git_bare(remote, &["rev-parse", name]);
    let next = test_box.git_bare(
        remote,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t.invalid",
            "commit-tree",
            &format!("{main}^{{tree}}"),
            "-p",
            &main,
            "-m",
            "the ref moves",
        ],
    );
    test_box.git_bare(remote, &["update-ref", name, &next, &main]);
    next
}

/// Whether `result` names the change `id`: as a row, or among the changes of other actors that a
/// listing only counts.
pub(crate) fn shows(result: &Value, id: &str) -> bool {
    let row = result["detail"]["changes"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|row| row["change"] == id));
    let counted = result["detail"]["more_changes"]["ids"]
        .as_array()
        .is_some_and(|ids| ids.iter().any(|one| one == id));
    row || counted
}

#[test]
fn a_read_inside_the_window_asks_the_remote_nothing() {
    let (test_box, _, remote, reader, id) = store();
    let (asked, first) = status(&test_box, &reader, None, OPEN, "first");
    assert_eq!(asked, 1, "the listing did not sweep: {first}");
    let main = first["detail"]["main"].clone();
    let next = move_trunk(&test_box, &remote);
    // A read a second later, so its own time is not the sweep's.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let (asked, served) = status(&test_box, &reader, None, OPEN, "served");
    assert_eq!(asked, 0, "the read inside the window asked the remote");
    assert_eq!(served["detail"]["main"], main, "{served}");
    assert_eq!(
        served["freshness"]["fetched_at"], first["freshness"]["fetched_at"],
        "the windowed read did not name the sweep's time"
    );
    assert!(shows(&served, &id), "{served}");
    let (_, named) = status(&test_box, &reader, Some(&id), OPEN, "named");
    assert!(shows(&named, &id), "{named}");

    let (asked, swept) = status(&test_box, &reader, None, "0", "closed");
    assert_eq!(asked, 1, "the read past the window did not sweep");
    assert_eq!(swept["detail"]["main"], next.as_str(), "{swept}");
}

#[test]
fn a_read_that_names_a_change_opens_the_window_too() {
    let (test_box, repo, remote, reader, id) = store();
    // Another live change, which the named read does not name: its sweep settles it too.
    let (other, _) = test_box.start_and_edit(&repo);
    test_box.git_ok(
        &reader,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/changes/*:refs/bay/changes/*",
        ],
    );
    let moved = move_ref(&test_box, &remote, &format!("refs/bay/changes/{other}"));
    let (asked, first) = status(&test_box, &reader, Some(&id), OPEN, "first");
    assert_eq!(asked, 1, "the named read did not sweep: {first}");
    let main = first["detail"]["main"].clone();
    move_trunk(&test_box, &remote);

    let (asked, listed) = status(&test_box, &reader, None, OPEN, "listed");
    assert_eq!(
        asked, 0,
        "the listing was not served by a named read's sweep"
    );
    assert_eq!(listed["detail"]["main"], main, "{listed}");
    let held = test_box.git_ok(
        &reader,
        &["rev-parse", &format!("refs/bay/changes/{other}")],
    );
    assert_eq!(
        held.trim(),
        moved,
        "the named read's sweep did not settle every live chain"
    );
}

#[test]
fn a_read_of_a_change_the_store_lacks_asks_the_remote() {
    let (test_box, repo, _, reader, _) = store();
    let (asked, first) = status(&test_box, &reader, None, OPEN, "first");
    assert_eq!(asked, 1, "the listing did not sweep: {first}");
    let (elsewhere, _) = test_box.start_and_edit(&repo);

    let (asked, named) = status(&test_box, &reader, Some(&elsewhere), OPEN, "named");
    assert_eq!(
        asked, 1,
        "the read of a chain the store lacks did not sweep"
    );
    assert!(shows(&named, &elsewhere), "{named}");
}

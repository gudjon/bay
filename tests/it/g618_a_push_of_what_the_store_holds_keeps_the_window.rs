//! G618 (a fleet finding, 2026-10-31: in a shared store most listings push a chain that local
//! work left ahead of the remote, and the push ended the window of the listing's own sweep, so 3
//! of 4 reads after one asked the remote again). A push from the store ends the window of a
//! listing's sweep only when the store may not hold what the remote now holds: the push failed,
//! or a ref it sent is no longer here at the object it sent. A push of what the store holds keeps
//! the window, and the write reads back from the store. Read from the trace a test build writes
//! when `BAY_TEST_GIT_TRACE` names a file (G171).
//!
//! Guards to mutate: in `src/sweep.rs`, check no ref a push sent, and the read after a push the
//! store no longer holds is served the old trunk; mark every push as one that ends the window, and
//! the read after a held push asks the remote; map no pushed ref to its local copy, and the read
//! after a push the store no longer holds is served. In `src/git.rs`, mark no push, or mark a
//! failed push as one that landed, and the read after a failed push is served the old trunk.

use crate::g617_a_listings_sweep_serves_the_reads_in_its_window::{
    OPEN, move_trunk, shows, status, store,
};
use crate::support;

use std::path::Path;

use serde_json::Value;
use support::{result_json, write_executable};

/// A listing in `repo`, then a change started there and queued to merge: the change id and the
/// trunk the listing read. Both writes push to the remote.
fn listed_then_queued(test_box: &support::TestBox, repo: &Path) -> (String, Value) {
    let (asked, first) = status(test_box, repo, None, OPEN, "first");
    assert_eq!(asked, 1, "the listing did not sweep: {first}");
    let (id, view) = test_box.start_and_edit(repo);
    let (code, queued) = support::result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    (id, first["detail"]["main"].clone())
}

#[test]
fn a_push_of_what_the_store_holds_keeps_the_window() {
    let (test_box, repo, remote, _, _) = store();
    let (id, main) = listed_then_queued(&test_box, &repo);
    move_trunk(&test_box, &remote);

    let (asked, after) = status(&test_box, &repo, None, OPEN, "after");
    assert_eq!(
        asked, 0,
        "the read after a push the store holds asked the remote"
    );
    assert_eq!(after["detail"]["main"], main, "{after}");
    assert!(shows(&after, &id), "the write did not read back: {after}");
}

#[test]
fn a_push_the_store_no_longer_holds_ends_the_window() {
    let (test_box, repo, remote, _, _) = store();
    let (id, _) = listed_then_queued(&test_box, &repo);
    // The store no longer holds the request the merge sent.
    test_box.git_ok(
        &repo,
        &["update-ref", "-d", &format!("refs/bay/requests/{id}")],
    );
    let next = move_trunk(&test_box, &remote);

    let (asked, after) = status(&test_box, &repo, None, OPEN, "after");
    assert_eq!(
        asked, 1,
        "the read after a push the store no longer holds was served by the sweep before it"
    );
    assert_eq!(after["detail"]["main"], next.as_str(), "{after}");
}

#[test]
fn a_failed_push_ends_the_window() {
    let (test_box, repo, remote, _, _) = store();
    let (asked, first) = status(&test_box, &repo, None, OPEN, "first");
    assert_eq!(asked, 1, "the listing did not sweep: {first}");
    let hook = remote.join("hooks").join("pre-receive");
    write_executable(&hook, "#!/bin/sh\nexit 1\n");
    let (code, refused) =
        result_json(
            test_box
                .bay(&repo)
                .args(["start", "CARD-REFUSED", "--paths", "src/"]),
        );
    assert_ne!(code, 0, "the remote took the start: {refused}");
    std::fs::remove_file(&hook).unwrap();
    let next = move_trunk(&test_box, &remote);

    let (asked, after) = status(&test_box, &repo, None, OPEN, "after");
    assert_eq!(
        asked, 1,
        "the read after a failed push was served by the sweep before it"
    );
    assert_eq!(after["detail"]["main"], next.as_str(), "{after}");
}

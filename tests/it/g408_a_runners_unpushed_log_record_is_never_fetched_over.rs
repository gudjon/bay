//! G408 (the no-work-is-lost rule, A10, 2026-10-27): a runner's log record whose push has not
//! reached the remote yet was lost at the next fetch of the logs, which forced every log ref,
//! this runner's own too, back to the remote's. This runner's own log is moved by the remote
//! only forward.
//!
//! Guard to mutate: `src/log.rs` `fetch`: fetch this runner's own log with the rest, forced.

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_record_not_yet_pushed_survives_a_fetch_of_the_logs() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let files = check_files("", &failing, 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let executor = test_box.clone_of(&remote, "executor");
    test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    let log = "refs/bay/log/local";
    let pushed = test_box.git_ok(&executor, &["rev-parse", log]);
    // A record appended here whose push has not reached the remote.
    let tree = test_box.git_ok(&executor, &["rev-parse", &format!("{pushed}^{{tree}}")]);
    let unpushed = test_box.git_ok(
        &executor,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t.invalid",
            "commit-tree",
            &tree,
            "-p",
            &pushed,
            "-m",
            "event check_failed evt_unpushed",
        ],
    );
    test_box.git_ok(&executor, &["update-ref", log, &unpushed]);
    // A command that fetches the logs.
    test_box.bay_ok(&executor, &["status"]);
    assert_eq!(
        test_box.git_ok(&executor, &["rev-parse", log]),
        unpushed,
        "the unpushed record was fetched over"
    );
}

//! G682 (a fleet finding, 2026-11-09: quovu7qa). An amendment whose `bay merge` read main
//! before the runner merged the change's first revision, and the change's chain after, was
//! appended as a new operation that no runner ever answers ("submitted" for good). A change
//! whose record holds a merged operation refuses a new revision `CHANGE_ALREADY_MERGED`, whose
//! recovery is a new change, whatever main this machine read.
//!
//! The guard to mutate: in `src/publish.rs` `submit_land`, the `merged_op` refusal (the
//! amendment is saved as a new operation).

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::queued::{Check, Trust, host_under};
use support::{PRODUCER, check_files, passing_result, result_json};

#[test]
fn an_amendment_sent_after_the_merge_is_refused() {
    let policy = check_files("x", &passing_result(1), 1, 120)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| text)
        .unwrap();
    let held = host_under(
        &Check {
            config: "[runner]\nlease_s = 30\nheartbeat_s = 30\n",
            policy: Some(&policy),
            seed: &[("src/a.rs", "fn a() {}\n")],
            hold_first: false,
            fails: "false",
        },
        Trust::Unsigned,
    );
    let exec = held.executor("exec-a", PRODUCER);
    let test_box = &held.test_box;
    let merge = |view: &PathBuf| {
        result_json(
            test_box
                .bay(view)
                .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
                .arg("merge"),
        )
    };

    // GIVEN revision 1 waiting for the runner
    let started = test_box.bay_ok(&held.repo, &["start", "one", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/one.rs"), "1\n").unwrap();
    let (code, first) = merge(&view);
    assert_eq!(code, 0, "{first}");

    // AND the runner merged revision 1, and this clone read the change's chain since, but not
    // main (the order `bay merge` reads them in, with a landing between the two reads)
    let ran = test_box.bay_ok(&exec, &["runner", "run", "--once"]);
    assert!(
        test_box.trunk_change_ids(&held.remote).contains(&id),
        "{ran}"
    );
    let chain = format!("refs/bay/changes/{id}");
    test_box.git_ok(
        &held.repo,
        &["fetch", "-q", "origin", &format!("+{chain}:{chain}")],
    );

    // WHEN an amendment is merged where main cannot be read now THEN it is refused as already
    // merged, and no operation is added
    let url = test_box.git_ok(&held.repo, &["remote", "get-url", "origin"]);
    test_box.git_ok(
        &held.repo,
        &["remote", "set-url", "origin", "/nonexistent/remote.git"],
    );
    std::fs::write(view.join("src/one.rs"), "2\n").unwrap();
    let (_, sent) = merge(&view);
    test_box.git_ok(&held.repo, &["remote", "set-url", "origin", url.trim()]);
    assert_eq!(sent["reason"], "CHANGE_ALREADY_MERGED", "{sent}");
    let status = test_box.bay_ok(&held.repo, &["status", &id]);
    assert_eq!(
        status["detail"]["changes"][0]["state"], "merged",
        "{status}"
    );
}

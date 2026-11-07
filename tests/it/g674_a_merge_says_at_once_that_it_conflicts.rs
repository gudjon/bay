//! G674 (a move of 4,154 files met six `MERGE_CONFLICT`s, each said only once its turn in the queue
//! came). The conflict needs only Git on the change and main, so `bay merge` says it on the spot,
//! names the files, and nothing waits in the queue. Bringing in main and resolving merges.
//!
//! Guard to mutate, in `src/publish.rs` `submit_land`: the `merged` call at admit (the merge answers
//! `queued`).

#![cfg(unix)]

use crate::support;

use support::{TestBox, UNCHECKED_POLICY, result_json};

const TEXT: &str = "one\ntwo\nthree\n";

#[test]
fn a_conflict_is_said_before_the_change_queues() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", TEXT)],
        &[(".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned())],
    );
    let remote = test_box.path().join("repo-remote.git");
    let other = test_box.clone_of(&remote, "other");
    let started = test_box.bay_ok(&repo, &["start", "mine", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(other.join("src/a.rs"), "one\nTHEIRS\nthree\n").unwrap();
    let (code, saved) = result_json(test_box.bay(&other).args(["save", "theirs"]));
    assert_eq!(code, 0, "{saved}");
    std::fs::write(view.join("src/a.rs"), "one\nMINE\nthree\n").unwrap();

    let (code, refused) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );

    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "MERGE_CONFLICT", "{refused}");
    assert_eq!(refused["detail"]["conflicts"][0], "src/a.rs", "{refused}");
    let queue = test_box.bay_ok(&repo, &["queue"]);
    assert!(
        !queue.to_string().contains(&format!("{id}.")),
        "nothing waits in the queue: {queue}"
    );
}

//! G672 (a change refused `UNDOES_MERGED` 13 times in a day, each answer after its turn in the
//! queue). The refusal needs no runner and no check, only Git on the change and main, so `bay merge`
//! gives it on the spot and nothing waits in the queue. A runner still decides at its own turn: main
//! may have moved since. Keeping the work and merging again goes through.
//!
//! Guard to mutate, in `src/publish.rs` `submit_land`: the `undoes_merged` call at admit (the merge
//! answers `queued`, and the agent hears only when a runner reaches it).

#![cfg(unix)]

use crate::support;

use support::{TestBox, UNCHECKED_POLICY, result_json};

const TEXT: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n";

#[test]
fn a_merge_is_refused_before_it_queues_and_merges_once_the_work_is_kept() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", TEXT)],
        &[(".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned())],
    );
    let remote = test_box.path().join("repo-remote.git");
    let other = test_box.clone_of(&remote, "other");
    let started = test_box.bay_ok(&repo, &["start", "older copy", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let shouted = TEXT.replace("two", "TWO");
    std::fs::write(other.join("src/a.rs"), &shouted).unwrap();
    let (code, saved) = result_json(test_box.bay(&other).args(["save", "shout"]));
    assert_eq!(code, 0, "{saved}");
    test_box.bay_ok(&repo, &["start", &id]);
    std::fs::write(view.join("src/a.rs"), format!("{TEXT}more\n")).unwrap();

    // No runner runs here, and the merge does not run the operation itself.
    let (code, refused) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );

    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "UNDOES_MERGED", "{refused}");
    assert_eq!(refused["detail"]["paths"][0], "src/a.rs", "{refused}");
    assert_eq!(refused["detail"]["change"], saved["change"], "{refused}");
    let queue = test_box.bay_ok(&repo, &["queue"]);
    assert!(
        !queue.to_string().contains(&format!("{id}.")),
        "nothing waits in the queue: {queue}"
    );
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let undoes = &status["detail"]["changes"][0]["operation"]["undoes"];
    assert_eq!(undoes["paths"][0], "src/a.rs", "{status}");

    std::fs::write(view.join("src/a.rs"), format!("{shouted}more\n")).unwrap();
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");
}

//! G775 (a measurement, 2026-11-17: 469 starts on a busy box took 23.4 s at the median, 18.1 s
//! of it on the network: four fetches, an `ls-remote` and one or two pushes, each a round trip
//! of 1.4 to 2.5 s whatever it carried). A start that opens a change asks the remote twice: one
//! fetch of everything it reads (trunk, the intent's claims, the shard index, the registry) and
//! one atomic push of everything it writes. Read from the trace a test build writes (G171).
//!
//! Guard to mutate: in `src/start.rs` `start`, open the repository with `Repo::open_fetched`
//! (the trunk alone) instead of the fetch that carries the start's reads, and the start fetches
//! again.

use crate::support;

use serde_json::Value;
use support::{TestBox, unchecked_files};

/// The network Git commands (`fetch`, `push`, `ls-remote`) one `bay start <args>` ran, each as
/// its argument list.
fn network_calls(test_box: &TestBox, repo: &std::path::Path, args: &[&str]) -> Vec<String> {
    let trace = test_box.path().join("start-trace.jsonl");
    let _ = std::fs::remove_file(&trace);
    let (code, started) = support::result_json(
        test_box
            .bay(repo)
            .arg("start")
            .args(args)
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{started}");
    let text = std::fs::read_to_string(&trace).unwrap_or_default();
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| matches!(line["sub"].as_str(), Some("fetch" | "push" | "ls-remote")))
        .map(|line| line["args"].to_string())
        .collect()
}

#[test]
fn a_start_fetches_once_and_pushes_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    // GIVEN a repository a start has read before
    test_box.bay_ok(&repo, &["start", "CARD-0", "--paths", "src/"]);

    // WHEN a new change starts
    let calls = network_calls(&test_box, &repo, &["CARD-1", "--paths", "src/"]);

    // THEN it asked the remote twice: one fetch and one push
    let fetches = calls
        .iter()
        .filter(|call| call.contains("\"fetch\""))
        .count();
    let pushes = calls
        .iter()
        .filter(|call| call.contains("\"push\""))
        .count();
    assert!(
        fetches == 1 && pushes == 1 && calls.len() == 2,
        "{} network calls:\n{}",
        calls.len(),
        calls.join("\n")
    );
}

#[test]
fn a_start_of_a_merged_intent_fetches_once_and_pushes_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    // GIVEN an intent whose first change merged
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/b.rs"), "fn b() {}\n").unwrap();
    let (code, merged) = support::result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");

    // WHEN the same intent starts again
    let calls = network_calls(&test_box, &repo, &["CARD-1", "--paths", "src/"]);

    // THEN the start read whether the first change merged from its one fetch
    let fetches = calls
        .iter()
        .filter(|call| call.contains("\"fetch\""))
        .count();
    let pushes = calls
        .iter()
        .filter(|call| call.contains("\"push\""))
        .count();
    assert!(
        fetches == 1 && pushes == 1 && calls.len() == 2,
        "{} network calls:\n{}",
        calls.len(),
        calls.join("\n")
    );
}

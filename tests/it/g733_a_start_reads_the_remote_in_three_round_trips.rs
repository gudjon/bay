//! G733 (a measurement, 2026-11-15: agents' `bay start` took 9.7 s at the median on a store read
//! by 51 sessions, five round trips to the remote one after another: trunk, the intent's claims,
//! the shard index, the registry and the push). A new change's start reads trunk, then the claims
//! on its intent, the shard index and the registry in one fetch, and pushes once: three round
//! trips. Read from the trace a test build writes (G171).
//!
//! Guard to mutate: in `src/start.rs` `start`, drop the call of `prefetch`, and the start fetches
//! the claims, the shard index and the registry each alone.

// The host hook is a shell script, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::{TestBox, check_files, passing_result};

#[test]
fn a_new_changes_start_asks_the_remote_three_times() {
    let test_box = TestBox::new();
    let files = check_files("true", &passing_result(1), 1, 60);
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);

    // WHEN a new change starts in a store that holds the registry
    let trace = test_box.path().join("start-trace.jsonl");
    let out = test_box
        .bay(&repo)
        .args(["start", "CARD-9", "--paths", "src/"])
        .env("BAY_TEST_GIT_TRACE", &trace)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // THEN it asked the remote three times: trunk, one fetch of the rest, and the push
    let text = std::fs::read_to_string(&trace).expect("the trace was written");
    let asks: Vec<String> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| matches!(line["sub"].as_str(), Some("fetch" | "ls-remote" | "push")))
        .map(|line| line["args"].to_string())
        .collect();
    assert_eq!(
        asks.len(),
        3,
        "the start asked the remote {}:\n{}",
        asks.len(),
        asks.join("\n")
    );
}

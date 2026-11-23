//! G734 (a measurement, 2026-11-15: agents' `bay merge` took 10.7 s at the median on a store read
//! by 51 sessions, seven round trips to the remote one after another, five of them reads that
//! need nothing of each other). A merge reads the listing of chains and requests, the actor's
//! talks, the registry and its chain side by side after trunk, then pushes and reads the queue:
//! four waves of remote calls, each wave's calls in flight at once. Read from the trace a test
//! build writes (G171).
//!
//! The chain read early is the one the chain read takes, so the merge fetches its chain once
//! (a 2026-11-23 measurement: the chain read had gained the lease refspec and `--prune` (G787)
//! and the read started early had not, so every merge fetched its chain twice, one after the
//! other, inside the wave tolerance below).
//!
//! Guards to mutate: in `src/publish.rs` `land`, drop the call of `read_ahead`, and the merge
//! reads the remote in seven waves; in `read_ahead`, start the chain fetch with arguments of its
//! own, and the chain is fetched twice.

// The host hook is a shell script, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::{TestBox, check_files, passing_result};

/// How many waves the remote calls of `trace` make: a call that starts after every call before
/// it ended begins a new wave.
fn waves(trace: &str) -> usize {
    let mut calls: Vec<(u64, u64)> = trace
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| matches!(line["sub"].as_str(), Some("fetch" | "ls-remote" | "push")))
        .filter_map(|line| {
            let end = line["end"].as_u64()?;
            Some((end - line["us"].as_u64()?, end))
        })
        .collect();
    calls.sort_unstable();
    let mut waves = 0;
    let mut ended = 0;
    for (start, end) in calls {
        if start >= ended {
            waves += 1;
        }
        ended = ended.max(end);
    }
    waves
}

#[test]
fn a_merge_reads_the_remote_in_four_waves() {
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
    let (id, view) = test_box.start_and_edit(&repo);

    // WHEN the change is merged
    let trace = test_box.path().join("merge-trace.jsonl");
    let out = test_box
        .bay(&view)
        .args(["merge"])
        .env("BAY_TEST_GIT_TRACE", &trace)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // THEN its remote calls make four waves: trunk, the reads side by side, the push, the queue.
    // Waves are judged by time: on a loaded box a fast read can end before its sibling's thread
    // starts, splitting one wave in two, so one split is allowed; reads one after another make
    // seven.
    let text = std::fs::read_to_string(&trace).expect("the trace was written");
    let made = waves(&text);
    assert!((4..=5).contains(&made), "{made} waves: {text}");

    // AND its chain is fetched once: the read started early is the one the chain read takes.
    let chain = format!("refs/bay/changes/{id}:");
    let chain_fetches = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["sub"] == "fetch")
        .filter(|line| {
            line["args"].as_array().is_some_and(|args| {
                args.iter()
                    .any(|arg| arg.as_str().is_some_and(|arg| arg.starts_with(&chain)))
            })
        })
        .count();
    assert_eq!(chain_fetches, 1, "{text}");
}

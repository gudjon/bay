//! G200b. An executor pass costs what its live work costs, not what its history holds.
//!
//! A pass with twenty closed changes and twenty finished queue places spawns as many `git`
//! processes, and as many fetches, as a pass with one of each, read from the trace a test
//! build writes when `BAY_TEST_GIT_TRACE` names a file (G171): every chain and every queue
//! ref is read in one batch, and a closed chain costs a fetch only once its retention cut may
//! be due. Once the retention window has passed, the pass removes the finished
//! (reservation, release) pairs below the head as whole pairs, keeps the highest pair, and
//! keeps the head (`DESIGN` § 10a).

use crate::support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

/// The most `git` spawns the larger history may add to a pass: none is expected; the slack
/// covers a process this test does not control, never one per closed change or queue place.
const HISTORY_SLACK: usize = 2;

/// A repository with `closed` abandoned changes, one landed change, `closed` finished queue
/// places (every one released, each naming the landed operation), and a live head above them
/// that names an open change. Returns the box, the remote and the executor's clone.
fn history(closed: u64) -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let (landed, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["decision"], "merged", "{result}");
    for _ in 0..closed {
        let (_, view) = test_box.start_and_edit(&repo);
        let (code, result) = result_json(test_box.bay(&view).args(["abandon"]));
        assert_eq!(code, 0, "{result}");
    }
    let (open, _) = test_box.start_and_edit(&repo);
    let handle = bay::repo::Repo::open(&repo).unwrap();
    let op = format!("{landed}.1");
    for place in 1..=closed {
        let seq = bay::queue::join(&handle, None, &op, &landed).unwrap();
        assert_eq!(seq, place);
        bay::queue::release(&handle, None, seq, &op, "published").unwrap();
    }
    let head = bay::queue::join(&handle, None, &format!("{open}.1"), &open).unwrap();
    assert_eq!(head, closed + 1);
    let executor = test_box.clone_of(&remote, "executor");
    (test_box, remote, executor)
}

/// The subcommands one `bay runner run --once` in `dir` spawned, by count, from bay's own
/// process only, and its result.
fn pass(test_box: &TestBox, dir: &Path, name: &str) -> (BTreeMap<String, usize>, Value) {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let (code, result) = result_json(
        test_box
            .bay(dir)
            .args(["runner", "run", "--once"])
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{result}");
    let text = std::fs::read_to_string(&trace).expect("the trace was written");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let mut counts = BTreeMap::new();
    for line in lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref())
    {
        *counts
            .entry(line["sub"].as_str().unwrap_or_default().to_owned())
            .or_insert(0) += 1;
    }
    (counts, result)
}

/// The queue refs on the remote.
fn queue_refs(test_box: &TestBox, remote: &Path) -> Vec<String> {
    test_box
        .git_bare(
            remote,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/bay/queue/",
                "refs/bay/queue-done/",
            ],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_pass_costs_the_same_with_one_or_twenty_closed_changes_and_queue_places() {
    let (one_box, _, one_executor) = history(1);
    let (many_box, many_remote, many_executor) = history(20);
    // The first pass of a new clone warms it; the second is the steady state that is measured.
    pass(&one_box, &one_executor, "one-warm");
    pass(&many_box, &many_executor, "many-warm");
    let (one, _) = pass(&one_box, &one_executor, "one");
    let (many, ran) = pass(&many_box, &many_executor, "many");
    let total = |counts: &BTreeMap<String, usize>| counts.values().sum::<usize>();
    assert!(
        total(&many) <= total(&one) + HISTORY_SLACK,
        "twenty closed changes and queue places cost {} git spawns, one costs {}: {many:?} against {one:?}",
        total(&many),
        total(&one)
    );
    assert_eq!(
        many.get("fetch"),
        one.get("fetch"),
        "fetches grow with history: {many:?} against {one:?}"
    );
    // Inside the retention window nothing is removed: every place is still on the remote.
    assert!(
        ran["detail"]["ran"]
            .as_array()
            .is_none_or(|ran| ran.iter().all(|step| step.get("queue_pruned").is_none())),
        "{ran}"
    );
    assert_eq!(queue_refs(&many_box, &many_remote).len(), 41);
}

#[test]
fn retention_removes_finished_pairs_below_the_head_and_keeps_the_highest_and_the_head() {
    let (test_box, remote, executor) = history(20);
    pass(&test_box, &executor, "warm");
    // The retention window passes at once.
    test_box.commit_config(
        &executor,
        "[retention]\nwindow_s = 0\n\n[runner]\nlease_s = 2\nheartbeat_s = 1\n",
    );
    let (_, ran) = pass(&test_box, &executor, "retention");
    let pruned: Vec<u64> = ran["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|step| step["queue_pruned"].as_array().cloned())
        .unwrap_or_else(|| panic!("no queue retention: {ran}"))
        .iter()
        .filter_map(Value::as_u64)
        .collect();
    assert_eq!(pruned, (1..=19).collect::<Vec<u64>>(), "{ran}");
    let mut kept = queue_refs(&test_box, &remote);
    kept.sort();
    assert_eq!(
        kept,
        vec![
            "refs/bay/queue-done/20".to_owned(),
            "refs/bay/queue/20".to_owned(),
            "refs/bay/queue/21".to_owned(),
        ],
        "the highest pair and the live head stay"
    );
    // The executor's own copy lost the pairs too, and the queue still reads its head.
    let mut local: Vec<String> = test_box
        .git_ok(
            &executor,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/bay/queue/",
                "refs/bay/queue-done/",
            ],
        )
        .lines()
        .map(str::to_owned)
        .collect();
    local.sort();
    assert_eq!(local, kept);
    let handle = bay::repo::Repo::open(&executor).unwrap();
    let queue = bay::queue::Queue::read(&handle).unwrap();
    assert_eq!(queue.head().map(|head| head.seq), Some(21));
    // A new reservation still goes above every number ever used.
    let next = bay::queue::join(&handle, None, "zzzzzzzz.1", "zzzzzzzz").unwrap();
    assert_eq!(next, 22);
}

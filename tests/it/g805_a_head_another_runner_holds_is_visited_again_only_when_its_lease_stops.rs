//! G805 (a fleet measurement, 2026-11-20: one box's runner answered 101 times an hour, 2.9 s
//! each, that another runner held one of two heads, a visit every 37 s while the holder's chain
//! stood still). A looping runner that found a head another runner holds does not visit it
//! again while the holder's lease, as each pass's fetch of the chains brings it, moves within a
//! lease, and the head's chain stands. Once the lease stood a lease, the next visit takes the head
//! over, as before (G696).
//!
//! The second case holds an operation outside the line (no live queue), which only a pass's own
//! visit takes over: a queue's head is also taken over at the start of a pass (G788), whatever
//! the stands say.
//!
//! Guards to mutate, in `src/executor.rs`: `holder_answered` answering `None` (each pass visits
//! the held head again); `held_until` not ending with the lease (a stand that holds an hour: the
//! operation of a holder that stopped is never taken over).

#![cfg(unix)]

use crate::support;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::queued::{Check, Held, OTHER_PRODUCER, host};

/// A change runner A holds in its check, beating its lease every second under a lease of 30 s,
/// longer than a pass on a loaded box, as a real one is, and runner B, looping beside it with its
/// journal and Git calls traced. With `queued` the change is the queue's head; without, it holds
/// no place (no queue is live). Returns the host, the change, runner A and runner B.
fn line(queued: bool) -> (Held, String, std::process::Child, std::process::Child) {
    let hosted = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let head = if queued {
        hosted.queue("slow", "src")
    } else {
        let started = hosted
            .test_box
            .bay_ok(&hosted.repo, &["start", "slow", "--paths", "src/"]);
        let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join("src/slow.rs"), "fn slow() {}\n").unwrap();
        let (code, submitted) = support::result_json(
            hosted
                .test_box
                .bay(&view)
                .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
                .arg("merge"),
        );
        assert_eq!(code, 0, "{submitted}");
        started["change"].as_str().unwrap().to_owned()
    };
    let a = hosted.executor("exec-a", support::PRODUCER);
    let b = hosted.executor("exec-b", OTHER_PRODUCER);
    hosted
        .test_box
        .git_ok(&b, &["config", "bay.runnerName", "exec-b"]);
    let holder = hosted.spawn(&a);
    hosted.wait_held();
    let dir = hosted.test_box.path();
    let looping = hosted
        .test_box
        .bay_process(&b)
        .args(["--json", "runner", "run", "--interval-s", "1"])
        .env("BAY_TEST_GIT_TRACE", dir.join("b-trace"))
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(dir.join("b-journal")).unwrap())
        .spawn()
        .unwrap();
    (hosted, head, holder, looping)
}

/// Runner B's visits that found `head` hosted, and its passes (each fetches every chain once).
fn seen(dir: &Path, head: &str) -> (usize, usize) {
    let journal = std::fs::read_to_string(dir.join("b-journal")).unwrap_or_default();
    let visits = journal
        .lines()
        .filter(|line| line.contains(&format!("{head}.1 [")) && line.contains("] ok ("))
        .count();
    let trace = std::fs::read_to_string(dir.join("b-trace")).unwrap_or_default();
    let passes = trace
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| {
            line["sub"] == "fetch" && line["args"].to_string().contains("refs/bay/changes/*")
        })
        .count();
    (visits, passes)
}

/// Waits until `done` holds, at most `within`.
fn until(within: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(began.elapsed() < within, "{what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn stop(mut child: std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_head_another_runner_holds_is_visited_once_while_its_lease_moves() {
    // GIVEN runner A holds the head in its check, and beats its lease
    let (hosted, head, holder, looping) = line(true);
    let dir = hosted.test_box.path().to_path_buf();
    // WHEN runner B passes six times beside it
    until(
        Duration::from_secs(120),
        "runner B never found the head held",
        || seen(&dir, &head).0 >= 1,
    );
    let first = seen(&dir, &head).1;
    until(
        Duration::from_secs(120),
        "runner B did not pass six times",
        || seen(&dir, &head).1 >= first + 6,
    );
    stop(looping);
    hosted.go();
    let _ = support::queued::finished(holder);
    // THEN it found the head held once
    let (visits, passes) = seen(&dir, &head);
    let journal = std::fs::read_to_string(dir.join("b-journal")).unwrap_or_default();
    assert_eq!(visits, 1, "{visits} visits in {passes} passes: {journal}");
}

#[test]
fn an_operation_whose_holder_stopped_is_taken_over_within_its_lease() {
    // GIVEN runner B found an operation runner A holds, outside the line
    let (hosted, head, holder, looping) = line(false);
    let dir = hosted.test_box.path().to_path_buf();
    until(
        Duration::from_secs(120),
        "runner B never found the head held",
        || seen(&dir, &head).0 >= 1,
    );
    // WHEN runner A stops, its lease with it
    let stopped = Instant::now();
    stop(holder);
    // THEN runner B claims it within a lease and a few passes of its last beat
    let chain = format!("refs/bay/changes/{head}");
    until(
        Duration::from_secs(180),
        "runner B never took the head over",
        || {
            hosted
                .test_box
                .git_bare(&hosted.remote, &["log", "--format=%s", &chain])
                .lines()
                .any(|subject| {
                    subject.starts_with(&format!("claim {head}.1 epoch"))
                        && subject.contains(" by exec-b ")
                })
        },
    );
    let took = stopped.elapsed();
    stop(looping);
    hosted.go();
    assert!(
        took < Duration::from_secs(30 + 60),
        "runner B took the head over {took:?} after its holder stopped"
    );
}

//! G796 (a fleet measurement, 2026-11-19: in one day 124 heads ran 5.5 h of checks on
//! candidates main had already moved past, and learned it only when their merging push was
//! refused; one 4-minute check went on 224 s after a change that passed its head landed 28 s
//! into it). A runner looks at main once a heartbeat while a candidate is checked. Once main
//! moved past the candidate's base, the check stops (its adapter is killed, and it keeps no
//! record), and the head builds again on the new main in the same visit: it lands on a check
//! of that candidate, never on the stopped one.
//!
//! Guards to mutate, in `src/publish.rs`:
//! - `watch_main` returning `None` (the head's check runs on after main moved: the visit ends
//!   only when the test lets the check go);
//! - `after_stopped_check` leaving the move off the chain (G552's test fails: another runner
//!   reads no move under the head, and a second document lands while the head checks again).

#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host, ran_for};
use support::{check_files, passing_result};

#[test]
fn a_check_main_moved_past_stops_and_the_head_lands_on_the_new_main() {
    // One pre-merge check, required only for src/: a document needs none, and passes the head.
    let policy = check_files("x", &passing_result(1), 1, 120)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| text.replacen("timeout_s", "when_paths = [\"src/**\"]\ntimeout_s", 1))
        .unwrap();
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 4\n",
        policy: Some(&policy),
        seed: &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
        hold_first: true,
        fails: "false",
    });
    let slow = held.queue("slow", "src");
    let doc = held.queue_with("doc", &["docs"], &[("docs/doc.md", "doc\n")]);
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    // The head's check is short of the hold window (G791), so the document may pass it.
    held.test_box
        .git_ok(&b, &["config", "bay.testPasserHoldS", "3600"]);

    // The head's first check runs, and holds until the test lets it go.
    let mut runner_a = held.spawn(&a);
    held.wait_held();
    // The document lands past it: main moves under the head's candidate.
    let pass = held.pass(&b);
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&doc),
        "the document did not pass the head: {pass}"
    );
    let moved = held.test_box.git_bare(&held.remote, &["rev-parse", "main"]);

    // The head's runner sees the move within a few heartbeats, stops the held check, and lands
    // the head on a check of a candidate built on the new main. The test never lets the first
    // check go while the runner may still end on its own.
    let began = Instant::now();
    let ended = loop {
        if runner_a.try_wait().unwrap().is_some() {
            break true;
        }
        if began.elapsed() > Duration::from_secs(60) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if !ended {
        held.go();
    }
    let ran = finished(runner_a);
    assert!(
        ended,
        "the head's check ran on after main moved past its candidate: {ran}"
    );
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&slow),
        "the head did not land: {ran}"
    );
    assert_eq!(
        ran_for(&ran, &slow)["decision"],
        "merged",
        "the head did not land in the visit that stopped its check: {ran}"
    );

    // Two runs: the stopped one on the old base, and the one the head landed on, on the new
    // main. The stopped run keeps no record.
    let requests = held.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_ne!(
        requests[0]["base"]["commit"],
        moved.as_str(),
        "{requests:?}"
    );
    assert_eq!(
        requests[1]["base"]["commit"],
        moved.as_str(),
        "{requests:?}"
    );
    let stopped_tree = requests[0]["candidate"]["tree"].as_str().unwrap();
    let notes = held.test_box.git_bare(
        &held.remote,
        &[
            "notes",
            &format!("--ref=refs/notes/bay/evidence/{PRODUCER}"),
            "list",
        ],
    );
    let stopped_note = held
        .test_box
        .git_bare(&held.remote, &["rev-parse", stopped_tree]);
    assert!(
        !notes.contains(&stopped_note),
        "the stopped check left a record: {notes}"
    );
    held.go();
}

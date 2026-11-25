//! G910 (a fleet finding, 2026-11-25: on one host in 30 h, runners checked a batch head for 10 to
//! 36 minutes after another runner took its claim, then claimed four to eleven members, found the
//! head gone only at its persist, and gave every member back while the head's new holder waited
//! for them). A runner whose lease beat finds that another runner took its operation stops its
//! checks at once, records nothing of them, and ends its visit as the remote's chain reads.
//!
//! Guard to mutate, in `src/publish.rs`: `taken_over` returning `false`.

#![cfg(unix)]

use crate::support;

use std::process::Command;
use std::time::{Duration, Instant};

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host, ran_for};

const CONFIG: &str = "[runner]\nlease_s = 2\nheartbeat_s = 1\n";

fn signal(child: &std::process::Child, name: &str) {
    let status = Command::new("kill")
        .args([name, &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill {name}");
}

#[test]
fn a_runner_another_runner_took_over_stops_its_checks() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let one = held.queue("one", "src");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let mut runner_a = held.spawn(&a);
    held.wait_held();
    // GIVEN runner A checks the head, and stops beating as a frozen runner does
    signal(&runner_a, "-STOP");
    std::thread::sleep(Duration::from_secs(65));
    // AND runner B takes the head over, and its run of the head holds
    held.hold_run(2);
    let runner_b = held.spawn(&b);
    held.wait_held_run(2);

    // WHEN A wakes, with its own check still held
    signal(&runner_a, "-CONT");

    // THEN A's check stops and its visit ends, though nobody lets its check go
    let start = Instant::now();
    while runner_a.try_wait().unwrap().is_none() {
        if start.elapsed() > Duration::from_secs(30) {
            held.go();
            held.go_run(2);
            let _ = finished(runner_b);
            panic!("A still checked a head B took over");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let pass_a = finished(runner_a);
    held.go_run(2);
    let pass_b = finished(runner_b);
    assert_eq!(
        ran_for(&pass_a, &one)["detail"]["superseded"],
        true,
        "{pass_a}"
    );
    // AND B's run of the head lands it, once, and A ran no other check
    assert_eq!(
        held.test_box.trunk_change_ids(&held.remote),
        vec![one.clone()],
        "{pass_b}"
    );
    assert_eq!(held.requests().len(), 2, "{pass_a}");
}

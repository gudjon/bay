//! G398 (a fleet finding, 2026-10-26: two runners worked one head; when the first runner's run
//! ended after the second had taken the head over, it gave the head's place back as `failed`,
//! and a head whose check had passed was never published). A runner gives a head's place back
//! only while its own claim stands, or once the operation has ended: a runner that lost the
//! head leaves the place to the runner that holds it.
//!
//! Guard to mutate, in `src/publish.rs`: `holds_place` returning `true`.

#![cfg(unix)]

use crate::support;

use std::process::Command;
use std::time::{Duration, Instant};

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host};

const CONFIG: &str = "[runner]\nlease_s = 2\nheartbeat_s = 1\n";

fn signal(child: &std::process::Child, name: &str) {
    let status = Command::new("kill")
        .args([name, &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill {name}");
}

#[test]
fn a_head_taken_over_mid_run_is_still_published() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        // Every run but the held first takes a while, so A's run ends while B's still runs.
        fails: "sleep 8; false",
    });
    let one = held.queue("one", "src");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let runner_a = held.spawn(&a);
    held.wait_held();
    // A stops beating: its chain and its presence go quiet, as a frozen runner's do.
    signal(&runner_a, "-STOP");
    std::thread::sleep(Duration::from_secs(65));
    let runner_b = held.spawn(&b);
    let start = Instant::now();
    while held.requests().len() < 2 {
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "B never took the head"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // B's run of the head is under way: A wakes, its held run passes, and its run ends first.
    signal(&runner_a, "-CONT");
    held.go();
    let _ = finished(runner_a);
    let pass = finished(runner_b);
    assert_eq!(
        held.test_box.trunk_change_ids(&held.remote),
        vec![one.clone()],
        "the head B held was not published: {pass}"
    );
}

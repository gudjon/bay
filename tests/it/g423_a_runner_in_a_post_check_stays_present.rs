//! G423. A runner inside a post check keeps its presence fresh.
//!
//! A runner beats its presence every heartbeat while it drives an operation, so the others
//! never read it as gone. Its own post checks ran outside that beat. Measured 2026-10-28, a
//! post check ran for minutes, the others read its runner as gone, and a runner that does not
//! serve the change's environment took it, found no worker, and the change waited out a
//! lease. Now a runner beats as it does inside an operation while its post work runs.
//!
//! The guard to mutate: the `beat.meanwhile` around the post work in `pass` (`src/executor.rs`);
//! call the work directly and the presence stands still while the check runs.

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};

use support::PRODUCER;
use support::queued::{Check, finished, host};

#[test]
fn a_runner_running_a_post_check_beats_its_presence() {
    // Run 1 is the change's own check; run 2, the post check on main's tip, takes five
    // seconds, and the test reads the runner's presence twice while it runs.
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\npost = true\n",
        policy: Some(
            "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\n\n[[check]]\nid = \"later\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"src/**\"]\n",
        ),
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "{ [ \"$n\" -ge 2 ] && sleep 5; false; }",
    });
    let one = held.queue("one", "src");
    let executor = held.executor("exec", PRODUCER);
    let runner = held.spawn(&executor);
    let start = Instant::now();
    while held.requests().len() < 2 {
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "the post check never started"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let presence = || {
        held.test_box
            .git_bare(
                &held.remote,
                &[
                    "for-each-ref",
                    "--format=%(objectname)",
                    "refs/bay/executor/",
                ],
            )
            .trim()
            .to_owned()
    };
    std::thread::sleep(Duration::from_millis(500));
    let before = presence();
    std::thread::sleep(Duration::from_secs(3));
    let during = presence();
    let pass = finished(runner);
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&one),
        "{pass}"
    );
    assert!(!before.is_empty(), "no presence was written: {pass}");
    assert_ne!(
        before, during,
        "the presence stood still while the post check ran: {pass}"
    );
}

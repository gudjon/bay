//! G750 (a fleet finding, 2026-11-16: two runners on one box visited each of 41 changes waiting
//! in line every pass, each visit 16 to 40 s of fetches, a push and a hundred local reads,
//! while the head checked for 86 minutes, and every visit answered `QUEUED` as the last had;
//! the box ran at load 100). A looping runner visits a change waiting in line once, and again
//! only once its own chain moved, the queue's head changed, main moved, or ten minutes passed:
//! when the head lands, the change behind it is visited again and lands.
//!
//! Guards to mutate: `src/executor.rs` `Parked::note`: park no `QUEUED` answer, and the waiting
//! change is visited every pass; `Parked::keep_stands`: keep every stand, and it never lands.

#![cfg(unix)]

use crate::support;

use std::io::{BufRead as _, BufReader};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use support::PRODUCER;
use support::queued::{Check, Held, host};

/// The journal lines so far that name operation `op_id`.
fn lines_of(journal: &Mutex<Vec<String>>, op_id: &str) -> Vec<String> {
    journal
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains(&format!("{op_id} ")))
        .cloned()
        .collect()
}

/// Waits up to `limit` for `done`.
fn until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while began.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    done()
}

/// A looping runner in `clone`, its journal (stderr) gathered line by line.
fn looping(held: &Held, clone: &Path) -> (std::process::Child, Arc<Mutex<Vec<String>>>) {
    let mut child = held
        .test_box
        .bay_process(clone)
        .args(["--json", "runner", "run", "--interval-s", "1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let journal = Arc::new(Mutex::new(Vec::new()));
    let gathered = Arc::clone(&journal);
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            gathered.lock().unwrap().push(line);
        }
    });
    (child, journal)
}

#[test]
fn a_change_waiting_in_line_is_visited_again_only_when_the_line_moves() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 60\nheartbeat_s = 1\nahead = 4\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let first = held.queue("first", "src");
    let second = held.queue("second", "lib");
    let op_id = format!("{second}.1");

    // One runner holds the head: its check waits until the test lets it go.
    let head_runner = held.executor("head-runner", PRODUCER);
    let mut holder = held.spawn(&head_runner);
    held.wait_held();

    // WHEN a second runner loops while the head checks
    let other = held.executor("other-runner", PRODUCER);
    let (mut child, journal) = looping(&held, &other);
    assert!(
        until(Duration::from_secs(60), || !lines_of(&journal, &op_id)
            .is_empty()),
        "the runner never visited the waiting change: {:?}",
        journal.lock().unwrap()
    );
    std::thread::sleep(Duration::from_secs(8));

    // THEN it visited the change behind the head once, though it passed many times
    let visits = lines_of(&journal, &op_id);
    assert_eq!(
        visits.len(),
        1,
        "a change waiting in line was visited again while nothing moved: {visits:?}"
    );
    assert!(visits[0].contains("QUEUED"), "{visits:?}");

    // AND once the head lands, the line moved: the change is visited again and lands
    held.go();
    let _ = holder.wait();
    let landed = until(Duration::from_secs(120), || {
        let ids = held.test_box.trunk_change_ids(&held.remote);
        ids.contains(&first) && ids.contains(&second)
    });
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        landed,
        "the change behind the head did not land: {:?} {:?}",
        held.test_box.trunk_change_ids(&held.remote),
        journal.lock().unwrap()
    );
}

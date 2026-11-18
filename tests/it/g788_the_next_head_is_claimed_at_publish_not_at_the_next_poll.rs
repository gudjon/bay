//! G788 (a fleet measurement, 2026-11-18, 1,831 landings in 72 hours: the change next in line was
//! claimed p50 58 to 79 s after a publish freed the head, p90 four to five minutes, and four
//! claims in five were another runner's, which learned of the new head only on its next pass).
//! A runner that may run the line's new head claims it within seconds of the publish that freed
//! it, whether it waits between passes or runs other work in a pass, never after its poll interval.
//! And the head's batch takes the places that joined behind it after its pass read the chains (in
//! 12 hours 20 of 55 batch plans passed over such places as "not read here").
//!
//! Guards to mutate: in `src/executor.rs`, `wait_for_line` sleeping the whole interval (the idle
//! runner claims the head a minute later); the probe between a pass's operations, or the
//! `waited` rule beside it (the busy runner claims it only after its pass and its wait); in
//! `src/publish.rs` `read_line_fetched`, the fetch of the unread places (the batch lands alone).

#![cfg(unix)]

use crate::support;

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use support::PRODUCER;
use support::queued::{Check, Held, Trust, host, host_under};

/// Two checks, one per environment: `src/` wakes the one in `env-a`, `lib/` the one in `env-b`.
const POLICY: &str = "revision = 1\n\n[policy]\nreuse_unsigned = true\n\n\
    [[check]]\nid = \"front\"\nenvironment = \"env-a\"\ncommand = \"scripts/bay-check\"\n\
    adapter = [\"scripts/bay-check\"]\nwhen_paths = [\"src/**\"]\nexpected_minimum = 1\ntimeout_s = 900\n\n\
    [[check]]\nid = \"back\"\nenvironment = \"env-b\"\ncommand = \"scripts/bay-check\"\n\
    adapter = [\"scripts/bay-check\"]\nwhen_paths = [\"lib/**\"]\nexpected_minimum = 1\ntimeout_s = 900\n";

/// The runner's wait between passes: far longer than any bound below.
const INTERVAL_S: &str = "900";

/// How soon after the publish the next head must be claimed: a pass's own reads and claim, on a
/// loaded box, and never a wait out of [`INTERVAL_S`] (seconds on an idle box).
const BOUND_S: u64 = 120;

/// An executor clone of the host named `name`, serving `environment` only.
fn runner(held: &Held, name: &str, environment: &str) -> PathBuf {
    let clone = held.executor(name, PRODUCER);
    let git = |args: &[&str]| held.test_box.git_ok(&clone, args);
    git(&["config", "bay.runnerName", name]);
    git(&["config", "bay.producerName", &format!("{name}-checks")]);
    git(&["config", "bay.checkEnvironment", environment]);
    clone
}

/// A looping runner in `clone`, its journal (stderr) gathered line by line.
fn looping(held: &Held, clone: &Path) -> (std::process::Child, Arc<Mutex<Vec<String>>>) {
    let mut child = held
        .test_box
        .bay_process(clone)
        .args(["--json", "runner", "run", "--interval-s", INTERVAL_S])
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

/// Waits up to `limit` for `done`.
fn until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while began.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}

/// The commit time (Unix seconds) of the first commit on change `id`'s chain at the host whose
/// subject starts with `subject`.
fn written(held: &Held, id: &str, subject: &str) -> Option<u64> {
    held.test_box
        .git_bare(
            &held.remote,
            &[
                "log",
                "--reverse",
                "--format=%ct %s",
                &format!("refs/bay/changes/{id}"),
            ],
        )
        .lines()
        .find_map(|line| {
            let (at, said) = line.split_once(' ')?;
            said.starts_with(subject).then(|| at.parse().ok()).flatten()
        })
}

/// The host, a line of `first` (`src/`, served by `exec-a`) and `second` (`lib/`, served by
/// `exec-b`), and `exec-a`'s one pass holding `first`'s check. `config` is `.bay/config.toml`.
fn line_with_held_head(config: &str) -> (Held, String, String, std::process::Child) {
    let held = host_under(
        &Check {
            config,
            policy: Some(POLICY),
            seed: &[("src/a.rs", "fn a() {}\n"), ("lib/b.rs", "fn b() {}\n")],
            hold_first: true,
            fails: "false",
        },
        Trust::Unsigned,
    );
    let first = held.queue("first", "src");
    let second = held.queue("second", "lib");
    let head_runner = runner(&held, "exec-a", "env-a");
    let holder = held.spawn(&head_runner);
    held.wait_held();
    (held, first, second, holder)
}

/// Lets `holder`'s check of `first` go, and waits for its pass to land `first`; returns when
/// the host's chain says it landed.
fn publish_first(held: &Held, holder: std::process::Child, first: &str) -> u64 {
    held.go();
    let out = holder.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    written(held, first, &format!("landed {first}.1")).unwrap_or_else(|| {
        panic!(
            "{first} did not land: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

/// Asserts that `second` is claimed by `exec-b` within [`BOUND_S`] of `landed`.
fn claimed_soon(held: &Held, second: &str, landed: u64, journal: &Mutex<Vec<String>>) {
    let claim = format!("claim {second}.1");
    let seen = until(Duration::from_secs(BOUND_S + 20), || {
        written(held, second, &claim).is_some()
    });
    let at = written(held, second, &claim);
    assert!(
        seen && at.is_some_and(|at| at.saturating_sub(landed) <= BOUND_S),
        "the next head was claimed {:?} s after the publish, not within {BOUND_S} s: {:?}",
        at.map(|at| at.saturating_sub(landed)),
        journal.lock().unwrap()
    );
}

#[test]
fn a_runner_waiting_between_passes_claims_the_next_head_at_publish() {
    let (held, first, second, holder) =
        line_with_held_head("[runner]\nlease_s = 60\nheartbeat_s = 1\n");

    // GIVEN the runner that serves the next change made its pass (it found the head another
    // runner's to check) and waits a quarter of an hour for the next
    let other = runner(&held, "exec-b", "env-b");
    let (mut child, journal) = looping(&held, &other);
    let visited =
        until(Duration::from_secs(180), || {
            journal.lock().unwrap().iter().any(|line| {
                line.contains(&format!("{first}.1")) && line.contains("CHECK_UNAVAILABLE")
            })
        });
    assert!(visited, "{:?}", journal.lock().unwrap());
    std::thread::sleep(Duration::from_secs(2));

    // WHEN another runner publishes the head, and cannot run the change behind it
    let landed = publish_first(&held, holder, &first);

    // THEN the waiting runner claims it at once, not at its next pass a quarter of an hour on
    claimed_soon(&held, &second, landed, &journal);
    let merged = until(Duration::from_secs(180), || {
        held.test_box
            .trunk_change_ids(&held.remote)
            .contains(&second)
    });
    let _ = child.kill();
    let _ = child.wait();
    assert!(merged, "{:?}", journal.lock().unwrap());
}

#[test]
fn a_runner_busy_in_its_pass_claims_the_next_head_between_operations() {
    let (held, first, second, holder) =
        line_with_held_head("[runner]\nlease_s = 60\nheartbeat_s = 1\nahead = 4\n");
    let third = held.queue("third", "lib");

    // GIVEN the runner that serves the next change checks it ahead of its turn, held
    held.hold_run(2);
    held.hold_run(3);
    let other = runner(&held, "exec-b", "env-b");
    let (mut child, journal) = looping(&held, &other);
    held.wait_held_run(2);

    // WHEN another runner publishes the head meanwhile, and that check ends
    let landed = publish_first(&held, holder, &first);
    held.go_run(2);

    // THEN the busy runner claims the new head next, in the same pass, before its next check
    // ahead (of the change behind) ends and before its next pass
    claimed_soon(&held, &second, landed, &journal);
    held.go_run(3);
    let merged = until(Duration::from_secs(180), || {
        let ids = held.test_box.trunk_change_ids(&held.remote);
        ids.contains(&second) && ids.contains(&third)
    });
    let _ = child.kill();
    let _ = child.wait();
    assert!(merged, "{:?}", journal.lock().unwrap());
}

#[test]
fn a_batch_takes_the_places_that_joined_after_its_pass_read_the_chains() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 60\nheartbeat_s = 30\nbatch = { min = 2, max = 8, max_failure_rate = 1.0 }\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let first = held.queue("first", "src");

    // GIVEN a runner whose pass read the chains, held at its visit of the head
    let clone = held.executor("exec-a", PRODUCER);
    let go = held.test_box.path().join("go-visit");
    let runner = held
        .test_box
        .bay_process(&clone)
        .env("BAY_TEST_PAUSE_AT", "visit-gated")
        .env("BAY_TEST_PAUSE_FILE", &go)
        .args(["--json", "runner", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = held.test_box.path().join("go-visit.paused");
    assert!(
        until(Duration::from_secs(120), || paused.exists()),
        "the runner never reached its visit of the head"
    );

    // WHEN three more changes take their places behind the head meanwhile
    let behind: Vec<String> = ["second", "third", "fourth"]
        .iter()
        .map(|name| held.queue(name, "src"))
        .collect();
    std::fs::write(&go, "go").unwrap();
    let out = runner.wait_with_output().unwrap();
    let journal = String::from_utf8_lossy(&out.stderr);

    // THEN the head's batch takes all of them: none is passed over as not read here
    assert!(out.status.success(), "{journal}");
    assert!(
        journal.contains(&format!("{first}.1 batch of 4 planned")),
        "the batch did not take the places behind it: {journal}"
    );
    assert!(!journal.contains("not read here"), "{journal}");
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    for id in std::iter::once(&first).chain(&behind) {
        assert!(
            on_main.contains(id),
            "{id} did not land: {on_main:?}\n{journal}"
        );
    }
}

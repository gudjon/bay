//! G480 (bay 0.6.0, lever S1; a fleet measurement, 2026-10-28). A runner places a change
//! submitted while its check runs, and refuses one it must refuse, before that check ends.
//!
//! A runner placed a submitted change only when its pass reached it, and a pass that ran a head
//! or a pre-check placed nothing for minutes: with four runners busy, a new change waited 2.7
//! min p50 for its place (dehurfyw: request 21:32:21Z, queued 21:36:55Z), and batches held 2.6
//! changes of 24. Now the runner's heartbeat thread visits each change submitted meanwhile,
//! once a heartbeat, as far as the queue gate: it gets its place, or its refusal, written as a
//! pass writes it. That thread never claims, checks or publishes.
//!
//! The guards to mutate: the `beside()` call in `Beat::meanwhile` (`src/presence.rs`): drop
//! it and nothing is placed until the check ends. The two `places_only` stops after the queue
//! gate in `publish::execute_op` (`src/publish.rs`): drop the first and a placement is checked
//! ahead of its turn beside the head's check; drop the second and a change submitted during
//! post work, with no queue live, is claimed and checked beside it.

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use support::PRODUCER;
use support::queued::{Check, Held, finished, host};
use support::result_json;

/// How long a test waits for what the runner does beside its check.
const BESIDE: Duration = Duration::from_secs(30);

/// Starts change `intent` on `dir/`, writes `path` in its view as `text`, and submits it with
/// `bay merge`, which only queues: no runner places it. Its change id and operation id.
fn submit(held: &Held, intent: &str, dir: &str, path: &str, text: &str) -> (String, String) {
    let started = held
        .test_box
        .bay_ok(&held.repo, &["start", intent, "--paths", dir]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), text).unwrap();
    let (code, submitted) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    (id, submitted["op_id"].as_str().unwrap().to_owned())
}

/// The host's chain messages of change `id`, newest first.
fn chain(held: &Held, id: &str) -> String {
    held.test_box.git_bare(
        &held.remote,
        &["log", "--format=%s", &bay::change::change_ref(id)],
    )
}

/// Whether `op_id` holds a live place in the host's queue, read from `repo`.
fn placed(repo: &Path, op_id: &str) -> bool {
    let opened = bay::repo::Repo::open(repo).unwrap();
    bay::queue::Queue::read(&opened)
        .unwrap()
        .of(op_id)
        .is_some()
}

#[test]
fn a_change_submitted_while_a_check_runs_is_placed_before_it_ends() {
    // `ahead = 1`, so a placement that went on past the queue gate would check the new change
    // ahead of its turn.
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let one = held.queue("one", "src");
    let executor = held.executor("exec", PRODUCER);
    let runner = held.spawn(&executor);
    // The head's check runs, and holds, until the test lets it go.
    held.wait_held();
    let began = Instant::now();
    let (two, two_op) = submit(&held, "two", "src/", "src/two.rs", "fn two() {}\n");
    let config = std::fs::read_to_string(held.repo.join(".bay/config.toml")).unwrap();
    let (three, three_op) = submit(
        &held,
        "three",
        ".bay/",
        ".bay/config.toml",
        &format!("not_a_bay_key = 1\n{config}"),
    );
    let mut placed_after = None;
    let mut refused_after = None;
    while began.elapsed() < BESIDE && (placed_after.is_none() || refused_after.is_none()) {
        if placed_after.is_none() && placed(&held.repo, &two_op) {
            placed_after = Some(began.elapsed());
        }
        if refused_after.is_none() && chain(&held, &three).contains("does not load") {
            refused_after = Some(began.elapsed());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // Read while the head's check still holds.
    let runs = held.requests().len();
    let two_chain = chain(&held, &two);
    held.go();
    let pass = finished(runner);
    assert!(
        placed_after.is_some(),
        "a change submitted while the head's check ran was not placed within {BESIDE:?}: {pass}"
    );
    assert!(
        refused_after.is_some(),
        "a change whose config does not load was not refused while the head's check ran: {}\n{pass}",
        chain(&held, &three)
    );
    assert_eq!(
        runs,
        1,
        "a check ran beside the head's own: {:?}\n{pass}",
        held.requests()
    );
    assert!(
        !two_chain.contains("claim"),
        "a placement claimed its change: {two_chain}"
    );
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&one),
        "{pass}"
    );
    assert!(
        !placed(&held.repo, &three_op),
        "a refused change holds a place"
    );
    eprintln!(
        "G480: placed {:?} and refused {:?} after submission, beside a check that had not ended",
        placed_after.unwrap(),
        refused_after.unwrap()
    );
}

#[test]
fn a_change_that_may_run_is_left_to_the_pass_beside_post_work() {
    // Run 1 is the first change's check; run 2, the post check on main's tip, takes ten
    // seconds. No queue is live meanwhile, so a change submitted then may run at once: only a
    // pass's own visit runs it.
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\npost = true\n",
        policy: Some(
            "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\n\n[[check]]\nid = \"later\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"src/**\"]\n",
        ),
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "{ [ \"$n\" -eq 2 ] && sleep 10; false; }",
    });
    let one = held.queue("one", "src");
    let executor = held.executor("exec", PRODUCER);
    let runner = held.spawn(&executor);
    let start = Instant::now();
    while held.requests().len() < 2 {
        assert!(start.elapsed() < BESIDE * 4, "the post check never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    let (two, _) = submit(&held, "two", "src/", "src/two.rs", "fn two() {}\n");
    let pass = finished(runner);
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&one),
        "{pass}"
    );
    assert!(
        !chain(&held, &two).contains("claim"),
        "a change was claimed beside the post check: {}\n{pass}",
        chain(&held, &two)
    );
    assert_eq!(
        held.requests().len(),
        2,
        "a check ran beside the post check: {pass}"
    );
}

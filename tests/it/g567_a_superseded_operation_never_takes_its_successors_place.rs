//! G567 (a fleet finding, 2026-10-29: grjioybr.1 landed over grjioybr.2). A superseded
//! operation never takes its successor's place in line, and never runs.
//!
//! A pass lists its operations when it begins and visits each one later. A change revised
//! meanwhile had its new operation placed by a heartbeat (S1), in its old operation's place;
//! the pass then visited the old operation, put it back in that place and landed it, and the
//! new revision was refused as already on main.
//!
//! The guards to mutate, in `src/publish.rs`: the `is_terminal()` stop in `execute_once` (drop
//! it, and a visit of an operation superseded while its change holds no place queues it
//! again); the `crate::queue::newer` stop in `queue_gate` (drop it, and an operation read from
//! a chain behind the queue replaces its successor's place).

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use support::queued::{Check, Held, finished, host};
use support::{EXECUTOR, PRODUCER};

/// How long a test waits for what the runner does beside its check.
const BESIDE: Duration = Duration::from_secs(30);

/// The host's chain messages of change `id`, newest first.
fn chain(held: &Held, id: &str) -> String {
    held.test_box.git_bare(
        &held.remote,
        &["log", "--format=%s", &bay::change::change_ref(id)],
    )
}

/// The operation the host's queue place `seq` names, or empty when no such place is live.
fn op_at(held: &Held, seq: u64) -> String {
    let out = held
        .test_box
        .git(&held.remote)
        .args(["show", &format!("refs/bay/queue/{seq}:entry.json")])
        .output()
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|entry| entry["op_id"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn checked(config: &'static str, hold_first: bool) -> Held {
    host(&Check {
        config,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first,
        fails: "false",
    })
}

/// One pass lists `one` and `two` and holds in one's check; meanwhile two is revised, and,
/// when `placed`, a heartbeat places the revision in two's place before the check ends. Then a
/// second pass. Asserts that main holds the revision and the first revision never ran or took
/// a place after its successor's.
fn revised_while_the_head_runs(config: &'static str, placed: bool) {
    let held = checked(config, true);
    let one = held.queue("one", "src");
    let two = held.queue("two", "src");
    let executor = held.executor("exec", PRODUCER);
    let runner = held.spawn(&executor);
    held.wait_held();
    let started = held.test_box.bay_ok(&held.repo, &["start", &two]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/two.rs"), "fn two() { 2; }\n").unwrap();
    held.test_box.bay_ok(&view, &["merge"]);
    let revised = format!("{two}.2");
    let old = format!("{two}.1");
    if placed {
        let began = Instant::now();
        while op_at(&held, 2) != revised {
            assert!(
                began.elapsed() < BESIDE,
                "the revision was not placed beside the head's check: {}",
                chain(&held, &two)
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    } else {
        assert_eq!(
            op_at(&held, 2),
            old,
            "a heartbeat placed the revision first"
        );
    }
    held.go();
    let pass = finished(runner);
    let after_first = chain(&held, &two);
    let next = held.pass(&executor);
    let two_chain = chain(&held, &two);
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&one),
        "{pass}"
    );
    assert!(
        !after_first.contains(&format!("landed {old}"))
            && !after_first.contains(&format!("claim {old} ")),
        "the pass ran the operation superseded since it began: {after_first}\n{pass}"
    );
    let landed = held
        .test_box
        .git_bare(&held.remote, &["show", "main:src/two.rs"]);
    assert_eq!(
        landed.trim(),
        "fn two() { 2; }",
        "main holds the superseded revision: {two_chain}\n{pass}\n{next}"
    );
    let queued = |op_id: &str| {
        two_chain
            .lines()
            .position(|line| line.starts_with(&format!("queued {op_id} ")))
    };
    // A place passed on to the revision writes no `queued` line of its own.
    let (newest, older) = (queued(&revised), queued(&old));
    assert!(
        older.is_none_or(|older| newest.is_some_and(|newest| newest < older)),
        "the superseded operation took a place after its successor's: {two_chain}"
    );
}

#[test]
fn an_operation_superseded_and_replaced_since_the_pass_began_is_never_run() {
    revised_while_the_head_runs("[runner]\nlease_s = 30\nheartbeat_s = 1\n", true);
}

#[test]
fn an_operation_superseded_since_the_pass_began_is_never_run() {
    // No heartbeat beats before the head's check ends, so the place still names the first
    // revision when the pass visits it.
    revised_while_the_head_runs("[runner]\nlease_s = 180\nheartbeat_s = 60\n", false);
}

#[test]
fn an_operation_read_behind_the_queue_leaves_its_successors_place() {
    let held = checked("[runner]\nlease_s = 30\nheartbeat_s = 1\n", false);
    let two = held.queue("two", "src");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    let queue = bay::queue::Queue::read(&opened).unwrap();
    let place = queue.of_change(&two).cloned().unwrap();
    // The queue already names the change's next operation; the chain this pass reads does not
    // yet hold it, as a chain read before the revision was submitted does not.
    let revised = format!("{two}.2");
    let signer = bay::signing::Signer {
        key: held.keys.executor.clone(),
        name: EXECUTOR.to_owned(),
    };
    assert!(bay::queue::replace(&opened, Some(&signer), &place, &revised).unwrap());
    let executor = held.executor("exec", PRODUCER);
    let pass = held.pass(&executor);
    let two_chain = chain(&held, &two);
    let old = format!("{two}.1");
    assert!(
        !held.test_box.trunk_change_ids(&held.remote).contains(&two),
        "the operation behind the queue ran: {two_chain}\n{pass}"
    );
    assert_ne!(
        op_at(&held, place.seq),
        old,
        "the operation behind the queue took its successor's place back: {pass}"
    );
    assert!(bay::queue::newer(&revised, &old));
    assert!(!bay::queue::newer(&old, &revised));
}

#[test]
fn an_operation_superseded_while_its_change_holds_no_place_never_takes_one() {
    let held = checked("[runner]\nlease_s = 30\nheartbeat_s = 1\n", false);
    let one = held.queue("one", "src");
    let two = held.queue("two", "src");
    let started = held.test_box.bay_ok(&held.repo, &["start", &two]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/two.rs"), "fn two() { 2; }\n").unwrap();
    held.test_box.bay_ok(&view, &["merge"]);
    let old = format!("{two}.1");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    let place = bay::queue::Queue::read(&opened)
        .unwrap()
        .of_change(&two)
        .cloned()
        .unwrap();
    assert_eq!(place.op_id, old, "a runner placed the revision first");
    // The place is released at its deadline before a runner passes it on to the revision, so
    // the queue names no operation of the change; a pass that listed the first revision before
    // it was superseded then visits it.
    let signer = bay::signing::Signer {
        key: held.keys.executor.clone(),
        name: EXECUTOR.to_owned(),
    };
    bay::queue::release(&opened, Some(&signer), place.seq, &old, "deadline").unwrap();
    let executor = held.executor("exec", PRODUCER);
    let exec = bay::repo::Repo::open(&executor).unwrap();
    // A pass fetches the chains before it visits their operations.
    exec.fetch_chains().unwrap();
    let visited =
        bay::publish::execute(&exec, &two, &old, &bay::publish::Step::new("read")).unwrap();
    let visited = visited.to_json().unwrap();
    let queue = bay::queue::Queue::read(&exec).unwrap();
    assert_eq!(
        queue.head().map(|head| head.change.clone()),
        Some(one),
        "{visited}"
    );
    assert!(
        queue.of(&old).is_none(),
        "the superseded operation took a new place: {visited}"
    );
    // The test placed the first revision itself, so the chain holds no `queued` line of it.
    let two_chain = chain(&held, &two);
    assert!(
        !two_chain.contains(&format!("queued {old} at ")),
        "the superseded operation was queued again: {two_chain}"
    );
}

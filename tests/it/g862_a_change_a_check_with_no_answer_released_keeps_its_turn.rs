//! G862 (a fleet finding, 2026-11-22: in one store, 4 of 4 changes whose place a check with no
//! clear answer released went to the back of the line when they were re-armed, one from the head
//! to behind 11 others): a check that gives no clear answer is not the change's fault, so the
//! change keeps its turn. When its operation takes a place again, the new place names the rank of
//! the place the check released, the line is in rank order, and the host refuses a rank that no
//! such release gives.
//!
//! The check gives no answer on its first run, for a tree with `unknown/`.
//!
//! Guards to mutate: in `src/queue.rs` `Entry::key`, order by number alone (the change lands
//! after the one that joined behind it); in `src/hook.rs` `kept_rank`, return `Ok` at once (the
//! host takes a forged rank); in `src/executor.rs` `kept_rank`, return `None` (the pass takes
//! the head first, and visits the change last); in `src/queue.rs` `release`, drop the local
//! `update-ref` (a place taken again here, after a release made here, keeps no rank).

#![cfg(unix)]

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use serde_json::json;

use crate::support;

use support::TestBox;
use support::queued::{Check, host, ran_for};
use support::{EXECUTOR, PRODUCER};

const CONFIG: &str = "[runner]\nlease_s = 450\nheartbeat_s = 120\n";

#[test]
fn a_change_a_check_with_no_answer_released_keeps_its_turn() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "{ [ -d unknown ] && [ \"$n\" -eq 1 ] && exit 0; false; }",
    });
    let exec = held.executor("exec", PRODUCER);
    // GIVEN `stuck` at the head of the line and `next` behind it
    let stuck = held.queue("stuck", "unknown");
    let next = held.queue("next", "src");
    // WHEN a pass gets no clear answer for `stuck`, and lands `next`, so main moves
    let ran = held.pass(&exec);
    assert_eq!(ran_for(&ran, &stuck)["reason"], "CHECK_UNKNOWN", "{ran}");
    assert_eq!(ran_for(&ran, &next)["decision"], "merged", "{ran}");
    // AND `later` joins the line after that
    let later = held.queue("later", "docs");
    // THEN `stuck`, asked again on the new main, takes its turn before `later`
    let mut ran = Vec::new();
    // Trunk's changes, oldest first.
    let landed = |id: &str| {
        held.test_box
            .trunk_change_ids(&held.remote)
            .iter()
            .rev()
            .position(|landed| landed == id)
    };
    for _ in 0..4 {
        if landed(&stuck).is_some() && landed(&later).is_some() {
            break;
        }
        ran.push(held.pass(&exec));
    }
    let (Some(stuck_at), Some(later_at)) = (landed(&stuck), landed(&later)) else {
        panic!("both land: {ran:?}");
    };
    assert!(
        stuck_at < later_at,
        "{stuck} lost its turn to {later}: {ran:?}"
    );
}

#[test]
fn the_host_refuses_a_rank_no_release_gives() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    // The runner's own key signs every record here, so only the rank can be refused.
    let runner = bay::signing::Signer {
        key: held.keys.executor.clone(),
        name: EXECUTOR.to_owned(),
    };
    let one = held.queue("one", "src");
    let two = held.queue("two", "docs");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    let queue = bay::queue::Queue::read(&opened).unwrap();
    let first = queue.of(&format!("{one}.1")).cloned().unwrap();
    // GIVEN the place of `one` released by a check that gave no clear answer
    bay::queue::release(
        &opened,
        Some(&runner),
        first.seq,
        &first.op_id,
        bay::queue::UNKNOWN,
    )
    .unwrap();

    // WHEN `two` takes a new place that names the rank of that place
    let forged = entry_commit(
        &held.test_box,
        &held.repo,
        &runner,
        &json!({"op_id": format!("{two}.1"), "change": two, "rank": first.rank, "after": first.seq}),
    );
    let out = held
        .test_box
        .git(&held.repo)
        .args([
            "push",
            "-q",
            "origin",
            &format!("{forged}:refs/bay/queue/9"),
        ])
        .output()
        .unwrap();
    // THEN the host refuses it
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a forged rank was taken");
    assert!(said.contains("keeps only the rank"), "{said}");

    // AND `one`, taking a place again, keeps its rank and is the head
    let again = bay::queue::join(&opened, Some(&runner), &first.op_id, &one).unwrap();
    let queue = bay::queue::Queue::read(&opened).unwrap();
    assert_eq!(queue.head().map(|head| head.seq), Some(again));
    assert_eq!(queue.position(again), 1);
}

/// A commit of one file `entry.json` holding `entry`, as a reservation's commit holds it,
/// signed by `signer`.
fn entry_commit(
    test_box: &TestBox,
    repo: &Path,
    signer: &bay::signing::Signer,
    entry: &serde_json::Value,
) -> String {
    let piped = |args: &[&str], input: &str| {
        let mut child = test_box
            .git(repo)
            .args(signer.config_args())
            .envs(signer.identity())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    let blob = piped(&["hash-object", "-w", "--stdin"], &format!("{entry}\n"));
    let tree = piped(&["mktree"], &format!("100644 blob {blob}\tentry.json\n"));
    piped(&["commit-tree", "-S", &tree, "-m", "reserve"], "")
}

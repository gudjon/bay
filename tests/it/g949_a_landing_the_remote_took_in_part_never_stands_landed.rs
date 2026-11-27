//! G949 (a fleet finding, 2026-11-27: GitHub refused a landing's push for main, `missing
//! necessary objects`, and took the chain and the terminal record that rode in the same atomic
//! push. `bay status` read the change merged, main never held it, and its author landed the
//! work again as a new change).
//!
//! A host checks each ref of a push before its transaction and commits the refs that pass, even
//! in an atomic push. Git's own receive-pack does so with a hidden ref, which this case uses to
//! refuse main and the landed markers as the host refused them. A refused landing whose chain the
//! remote took is taken back there at once: the remote's chain never says landed for a commit
//! main lacks, and its author's merge lands the change once main takes it.
//!
//! G931's batch of six, whose chains all said landed while main never held them, is the same
//! fault in a batch's push; each member is taken back. A bay host takes no part of such a push:
//! its `pre-receive` hook sees the push without the refs Git refused there, and declines the
//! rest. So the batch case drops that hook, and its host takes what GitHub took.
//!
//! Guard to mutate: in `src/publish.rs`, `chain_taken_back` takes the record back when the
//! remote's chain is the landing's head (make it return at once, and the remote's chain says
//! landed).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::Value;

use support::PRODUCER;
use support::queued::{Check, host};
use support::{TestBox, check_files, passing_result, result_json};

/// The change record of `id` as `git` reads it.
fn chain(git: &dyn Fn(&[&str]) -> String, id: &str) -> Value {
    serde_json::from_str(&git(&[
        "show",
        &format!("refs/bay/changes/{id}:change.json"),
    ]))
    .unwrap()
}

/// How many times the remote's main holds change `id`.
fn on_main(test_box: &TestBox, remote: &Path, id: &str) -> usize {
    test_box
        .trunk_change_ids(remote)
        .iter()
        .filter(|landed| **landed == id)
        .count()
}

/// Makes the remote refuse main and the landed markers, and take every other ref of a push.
fn refuse_main(test_box: &TestBox, remote: &Path) {
    for hidden in [
        "refs/heads/main",
        "refs/bay/landed",
        "refs/bay/landed-intent",
    ] {
        test_box.git_bare(remote, &["config", "--add", "receive.hideRefs", hidden]);
    }
}

/// Drops the bay host's `pre-receive` hook from `remote`, so it judges a push as a host with no
/// hook of bay's does (GitHub).
fn hookless(test_box: &TestBox, remote: &Path) {
    for key in ["hook.bay-scope.command", "hook.bay-scope.event"] {
        test_box.git_bare(remote, &["config", "--unset-all", key]);
    }
}

/// Asserts the remote's chain of `id` says neither that it landed nor that an operation
/// published, and says its landing was taken back.
fn taken_back(test_box: &TestBox, remote: &Path, id: &str) {
    let remote_git = |args: &[&str]| test_box.git_bare(remote, args);
    let record = chain(&remote_git, id);
    assert_ne!(record["state"], "landed", "{record}");
    let ops = record["operations"].as_array().unwrap();
    assert!(ops.iter().all(|op| op["state"] != "published"), "{record}");
    let log = remote_git(&["log", "--format=%s", &format!("refs/bay/changes/{id}")]);
    assert!(log.contains("taken back"), "{log}");
}

#[test]
fn a_landing_the_remote_took_in_part_is_taken_back_there_and_lands_once() {
    // GIVEN a queued change with one passing check, and a remote that refuses main and the
    // landed markers and takes every other ref of a push
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let remote_git = |args: &[&str]| test_box.git_bare(&remote, args);
    refuse_main(&test_box, &remote);

    // WHEN a runner lands it
    let e1 = test_box.clone_of(&remote, "e1");
    test_box.git_ok(&e1, &["config", "bay.producerName", "e1-checks"]);
    let (_, ran) = result_json(test_box.bay(&e1).args(["runner", "run", "--once"]));

    // THEN main lacks it, and the remote's chain says neither that it landed nor that its
    // operation published
    assert_eq!(on_main(&test_box, &remote, &id), 0, "{ran}");
    taken_back(&test_box, &remote, &id);

    // AND once the remote takes main again, its author's merge and a runner land it, once
    remote_git(&["config", "--unset-all", "receive.hideRefs"]);
    let (code, merged) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{merged}");
    for _ in 0..4 {
        if on_main(&test_box, &remote, &id) > 0 {
            break;
        }
        let (code, ran) = result_json(test_box.bay(&e1).args(["runner", "run", "--once"]));
        assert_eq!(code, 0, "{ran}");
    }
    assert_eq!(on_main(&test_box, &remote, &id), 1);
    assert_eq!(chain(&remote_git, &id)["state"], "landed");
}

#[test]
fn a_batch_the_remote_took_in_part_is_taken_back_on_each_member() {
    // GIVEN two changes in line that batch, and a remote with no hook of bay's that refuses main
    // and the landed markers
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\nbatch = { min = 2, max = 8 }\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "[ -e src/bad.rs ]",
    });
    let ids = [held.queue("one", "src"), held.queue("two", "src")];
    hookless(&held.test_box, &held.remote);
    refuse_main(&held.test_box, &held.remote);

    // WHEN a runner passes
    let executor = held.executor("exec", PRODUCER);
    let pass = held.pass(&executor);

    // THEN main holds neither, and each member's chain on the remote is taken back
    for id in &ids {
        assert_eq!(on_main(&held.test_box, &held.remote, id), 0, "{pass}");
        taken_back(&held.test_box, &held.remote, id);
    }
}

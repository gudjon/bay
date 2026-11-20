//! G819 (a fleet finding, 2026-11-20: for 14 minutes nothing merged while the queue's head
//! needed checks in two environments that no live runner served together. Each runner served
//! one of them, found the other served by a live runner elsewhere, and left the head in its
//! place for that runner, which did the same, so no runner took it). A runner leaves the head to
//! another live runner only when that runner's machine serves every environment the head's
//! checks need, not only the ones this machine lacks. When none does, a runner takes the head,
//! finds no worker, and the head leaves the line, so the changes behind it merge.
//!
//! Guard to mutate, in `src/publish.rs`: `served_elsewhere` asking for a live runner that
//! serves only the environments this machine lacks.

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::queued::{Check, Held, Trust, host_under};
use support::{PRODUCER, check_files, passing_result};

/// A pre-merge check `web` on `web/**`, in `env-b`.
const WEB: &str = "\n[[check]]\nid = \"web\"\nenvironment = \"env-b\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"web/**\"]\n";

/// A runner `exec-<name>` on machine `box-<name>`, which serves `environment` only.
fn runner(held: &Held, name: &str, environment: &str) -> PathBuf {
    let clone = held.executor(&format!("exec-{name}"), PRODUCER);
    let test_box = &held.test_box;
    test_box.git_ok(&clone, &["config", "bay.machine", &format!("box-{name}")]);
    test_box.git_ok(
        &clone,
        &["config", "bay.runnerName", &format!("exec-{name}")],
    );
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("exec-{name}-checks")],
    );
    test_box.git_ok(
        &clone,
        &["config", "--add", "bay.checkEnvironment", environment],
    );
    clone
}

#[test]
fn a_head_no_live_runner_serves_whole_leaves_the_line() {
    // `tests` in env-a on every path, and `web` in env-b on web/.
    let policy = check_files("true", &passing_result(1), 1, 120)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| format!("{}{WEB}", text.replace("\"local\"", "\"env-a\"")))
        .unwrap();
    let held = host_under(
        &Check {
            config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
            policy: Some(&policy),
            seed: &[("src/a.rs", "fn a() {}\n"), ("web/a.ts", "a\n")],
            hold_first: false,
            fails: "false",
        },
        Trust::Unsigned,
    );
    // GIVEN a live runner for env-a and one for env-b, and none for both
    let a = runner(&held, "a", "env-a");
    let b = runner(&held, "b", "env-b");
    let _ = held.pass(&a);
    let _ = held.pass(&b);
    // AND at the head a change on src/ and web/, which needs both, and behind it one on src/
    let both = held.queue_with(
        "both",
        &["src", "web"],
        &[("src/b.rs", "fn b() {}\n"), ("web/b.ts", "b\n")],
    );
    let one = held.queue_with("one", &["src"], &[("src/c.rs", "fn c() {}\n")]);

    // WHEN each runner passes twice
    let mut passes = Vec::new();
    for _ in 0..2 {
        passes.push(held.pass(&a));
        passes.push(held.pass(&b));
    }

    // THEN the head recorded its wait for a worker and left the line, and the change behind it
    // merged
    let chain = held.test_box.git_bare(
        &held.remote,
        &["log", "--format=%s", &format!("refs/bay/changes/{both}")],
    );
    assert!(
        chain.lines().any(|line| line.starts_with("unavailable ")),
        "{both} never recorded its wait:\n{chain}\n{passes:#?}"
    );
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    assert!(on_main.contains(&one), "{one} did not merge: {passes:#?}");
    assert!(!on_main.contains(&both), "{passes:#?}");
}

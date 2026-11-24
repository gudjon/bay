//! G888 (BACKLOG 278; a fleet finding, 2026-11-24: a change needed checks in two environments,
//! no live runner served both, and in 20 minutes the runners claimed it 25 times. Each runner
//! served the environment the last wait named, so it took the operation over when the lease of
//! the runner that recorded the wait lapsed, found no worker for the other environment and named
//! that one). A runner whose box does not serve every environment the change still needs passes
//! over its wait for a worker and writes nothing, as one runner runs every check of a candidate
//! (G819). A runner whose box serves them all takes it.
//!
//! Guard to mutate, in `src/publish.rs`: `waits_for_worker` passing over only when this box
//! lacks an environment the wait names (`partial` always false).

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;
use std::time::Duration;

use support::queued::{Check, Held, Trust, host_under};
use support::{PRODUCER, check_files, passing_result};

/// A pre-merge check `web` on `web/**`, in `env-b`.
const WEB: &str = "\n[[check]]\nid = \"web\"\nenvironment = \"env-b\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"web/**\"]\n";

/// Longer than the lease's takeover window (`takeover_s` in the config below).
const LAPSE: Duration = Duration::from_secs(7);

/// A runner `exec-<name>` on machine `box-<name>`, which serves `environments`.
fn runner(held: &Held, name: &str, environments: &[&str]) -> PathBuf {
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
    for environment in environments {
        test_box.git_ok(
            &clone,
            &["config", "--add", "bay.checkEnvironment", environment],
        );
    }
    clone
}

#[test]
fn a_change_no_runner_serves_whole_is_claimed_once() {
    // `tests` in env-a on every path, and `web` in env-b on web/.
    let policy = check_files("true", &passing_result(1), 1, 120)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| format!("{}{WEB}", text.replace("\"local\"", "\"env-a\"")))
        .unwrap();
    let held = host_under(
        &Check {
            config: "[runner]\nlease_s = 5\ntakeover_s = 6\nheartbeat_s = 1\n",
            policy: Some(&policy),
            seed: &[("src/a.rs", "fn a() {}\n"), ("web/a.ts", "a\n")],
            hold_first: false,
            fails: "false",
        },
        Trust::Unsigned,
    );
    // GIVEN a live runner for env-a and one for env-b, and none for both
    let a = runner(&held, "a", &["env-a"]);
    let b = runner(&held, "b", &["env-b"]);
    let _ = held.pass(&a);
    let _ = held.pass(&b);
    // AND a change on src/ and web/, which needs both
    let both = held.queue_with(
        "both",
        &["src", "web"],
        &[("src/b.rs", "fn b() {}\n"), ("web/b.ts", "b\n")],
    );

    // WHEN the runners pass in turn, each after the last one's lease could be taken over
    let mut passes = Vec::new();
    for turn in [&a, &b, &a] {
        passes.push(held.pass(turn));
        std::thread::sleep(LAPSE);
    }

    // THEN one runner claimed it and recorded its wait, and no other took it over
    let chain = || {
        held.test_box.git_bare(
            &held.remote,
            &["log", "--format=%s", &format!("refs/bay/changes/{both}")],
        )
    };
    let first = chain();
    let claims = first
        .lines()
        .filter(|line| line.starts_with("claim "))
        .count();
    assert_eq!(claims, 1, "{first}\n{passes:#?}");
    assert!(
        first.lines().any(|line| line.starts_with("unavailable ")),
        "{both} never recorded its wait:\n{first}\n{passes:#?}"
    );

    // WHEN a runner whose box serves both passes
    let c = runner(&held, "c", &["env-a", "env-b"]);
    let ran = vec![held.pass(&c), held.pass(&c)];
    // THEN it takes the change, and the change merges
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        on_main.contains(&both),
        "{both} did not merge:\n{}\n{ran:#?}",
        chain()
    );
}

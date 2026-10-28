//! G443. A landing reads nothing after its own publishing push that only a later visit needs.
//!
//! A single-change head run made 18-20 remote git calls on a fleet's GitHub remote, nine of them
//! in the publish step (2026-10-28, BACKLOG 99), at 1.2-1.5 s each. After the push that landed
//! the change, the terminal record fetched every runner's log to look for a record of the same
//! landing, and the run fetched the change's chain and the queue to release a place the push
//! had already released. Neither can find anything after this process's own push landed the
//! change: that push would have been refused. Now neither read is made.
//!
//! The guards to mutate: `write_terminal`'s `if ctx.pushed_landing.is_none()` around
//! `log::fetch`, and the `ctx.pushed_landing.is_none()` in `execute_op`'s release of the head's
//! place, before its `holds_place` refetch of the change's chain (`src/publish.rs`).

// Each case runs shell scripts (the adapter, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::PRODUCER;
use support::queued::{Check, host, ran_for};
use support::{TestBox, write_executable};

/// Puts a `git` on the box's PATH that writes every call's arguments, one line each, to the
/// returned file, then runs the real git.
fn trace_git(test_box: &TestBox) -> PathBuf {
    let out = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    let trace = test_box.path().join("git-trace");
    write_executable(
        &test_box.path().join("fakebin/git"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec \"{real}\" \"$@\"\n",
            trace.display()
        ),
    );
    trace
}

/// The git calls after the publishing push (`push -q --atomic`) that fetch `pattern`.
fn fetches_after_publish(trace: &Path, pattern: &str) -> Vec<String> {
    let text = std::fs::read_to_string(trace).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let pushed = lines
        .iter()
        .position(|line| line.contains("push -q --atomic"))
        .unwrap_or_else(|| panic!("no publishing push in the trace"));
    lines[pushed + 1..]
        .iter()
        .filter(|line| line.starts_with("fetch") || line.contains(" fetch "))
        .filter(|line| line.contains(pattern))
        .map(|line| (*line).to_owned())
        .collect()
}

#[test]
fn a_landing_fetches_no_log_and_no_place_after_its_own_push() {
    // A change with a place in the queue, as an executor queues it, so its head run releases
    // that place in the publishing push.
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\npost = false\nbisect_post = false\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "[ -e src/bad.rs ]",
    });
    let id = held.queue("one", "src");
    let executor = held.executor("exec", PRODUCER);
    let trace = trace_git(&held.test_box);
    let pass = held.pass(&executor);
    assert_eq!(ran_for(&pass, &id)["decision"], "merged", "{pass}");
    let logs = fetches_after_publish(&trace, "refs/bay/log/*");
    assert!(
        logs.is_empty(),
        "the landing fetched every runner's log after its own push: {logs:?}"
    );
    let chain = fetches_after_publish(&trace, &format!("refs/bay/changes/{id}"));
    assert!(
        chain.is_empty(),
        "the landing fetched its chain again to release a place its push released: {chain:?}"
    );
}

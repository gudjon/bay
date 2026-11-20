//! G826 (a fleet finding, 2026-11-20: 95 of 557 `red_owner` events in eight days were written
//! again by a second or third runner, each before it could read the first one's log, and each
//! woke the session again). Of the live runners that serve a red check's environment, one tells
//! the red: the one whose name weighs most with the check and its environment, as G822 chooses the
//! runner of a post check. Another runner tells it once that runner's presence reads stale.
//!
//! Guard to mutate, in `src/red_owner.rs` `told`: the `teller` test (drop it and the runner
//! that does not own the check tells the red beside its live owner).

#![cfg(unix)]

use crate::g822_a_post_check_runs_on_one_live_runner::{land, ran_post, weight};
use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{EXECUTOR, TestBox, check_files, passing_result};

/// The wake adapter: appends each request to `$HOME/woken`. bay gives an adapter only PATH and
/// HOME.
const WAKE: &str = "#!/bin/sh\ncat >> \"$HOME/woken\"\necho >> \"$HOME/woken\"\nprintf '{\"schema\":\"bay.wake-result/1\",\"result\":\"woken\",\"reason\":null}'\n";

/// Two runner clones of one remote that serve one environment, and the post check `suite`, which
/// fails on every tree that holds `src/l1.rs`. Each runner writes its presence on the tests' 1 s
/// cadence, so it reads live for 33 s. Returns the clone whose name weighs most with `suite`,
/// then the other.
fn two_runners(test_box: &TestBox) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let mut files = check_files(
        &format!("[ \"$1\" = suite ] && [ -e src/l1.rs ] && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str("\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 600\nwhen_paths = [\"src/**\"]\n");
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\nbisect_post = false\n\n[plugin.wake]\ncommand = \"scripts/bay-wake\"\n".to_owned(),
    ));
    files.push(("scripts/bay-wake".to_owned(), WAKE.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let other = test_box.clone_of(&remote, "other");
    test_box.git_ok(&other, &["config", "bay.runnerName", "other"]);
    test_box.git_ok(&other, &["config", "bay.producerName", "other-checks"]);
    if weight(EXECUTOR) > weight("other") {
        (repo, other)
    } else {
        (other, repo)
    }
}

/// The `red` wake requests the adapter was given, oldest first.
fn red_wakes(test_box: &TestBox) -> Vec<Value> {
    std::fs::read_to_string(test_box.home.join("woken"))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|request| request["cause"] == "red")
        .collect()
}

/// suite passes on `src/s0.rs`'s landing, and each runner has made a pass; then `src/l1.rs`
/// lands and the owner of suite runs it there: it fails, and no one is told yet.
fn red(test_box: &TestBox, owner: &Path, other: &Path) {
    land(test_box, owner, "src", "src/s0.rs");
    assert_eq!(ran_post(test_box, owner, 0), "suite");
    assert_eq!(ran_post(test_box, other, 0), "");
    land(test_box, owner, "src", "src/l1.rs");
    assert_eq!(ran_post(test_box, owner, 0), "suite");
    assert_eq!(red_wakes(test_box).len(), 0);
}

#[test]
fn a_red_is_told_by_the_runner_that_owns_its_check_alone_while_that_runner_is_live() {
    let test_box = TestBox::new();
    let (owner, other) = two_runners(&test_box);
    red(&test_box, &owner, &other);

    // WHEN the runner that does not own suite makes a pass, THEN it tells no one.
    ran_post(&test_box, &other, 0);
    let wakes = red_wakes(&test_box);
    assert_eq!(
        wakes.len(),
        0,
        "a runner told a red its live owner tells: {wakes:?}"
    );

    // WHEN the owner makes a pass, THEN it tells `src/l1.rs`'s change, once.
    ran_post(&test_box, &owner, 0);
    ran_post(&test_box, &other, 0);
    ran_post(&test_box, &owner, 0);
    let wakes = red_wakes(&test_box);
    assert_eq!(wakes.len(), 1, "{wakes:?}");
    let line = wakes[0]["line"].as_str().unwrap();
    assert!(line.contains("src/l1.rs"), "{line}");
}

#[test]
fn a_red_whose_owner_reads_stale_is_told_by_another_runner() {
    let test_box = TestBox::new();
    let (owner, other) = two_runners(&test_box);
    red(&test_box, &owner, &other);

    // WHEN the owner stops: 300 s on, its presence reads stale, THEN the other runner tells the
    // red, once.
    ran_post(&test_box, &other, 300);
    let wakes = red_wakes(&test_box);
    assert_eq!(
        wakes.len(),
        1,
        "a red waited on a runner whose presence reads stale: {wakes:?}"
    );
    ran_post(&test_box, &owner, 0);
    assert_eq!(red_wakes(&test_box).len(), 1, "a red is told once");
}

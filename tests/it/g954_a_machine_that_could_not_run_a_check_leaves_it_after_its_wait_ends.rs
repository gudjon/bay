//! G954 (a fleet finding, 2026-11-28: a machine could not run a change's check and handed it on
//! (G950); a runner on a third machine claimed it, which ends the wait (G690), and gave it back on
//! a fault; a runner on the first machine then read no wait, took the change again and ran its
//! check 25 minutes to the same `infra_failure`). The machines that could not run a check are
//! named on the operation, and a runner on one of them passes it over while no wait stands too,
//! as long as a live runner on another machine serves it. A runner on a machine of that list is
//! no one to hand it to, either: with only such runners live, the pass blocks as before.
//!
//! The adapter's unit `node` is an `infra_failure` when the declared `PROBE` variable says
//! `nonode`, and passes otherwise.
//!
//! Guards to mutate, in `src/publish.rs`: `waits_for_worker` returning at once with no wait (the
//! first case runs the change again on bad-box), and `live_runner_elsewhere` counting a runner on
//! a machine of `unable` (the second case hands the change back to bad-box).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json};

/// The policy: one check `tests` of two units, which declares `PROBE`.
const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 2\ntimeout_s = 60\nenv = [\"PROBE\"]\n";

/// The adapter. Its unit `node` is an `infra_failure` while `PROBE` says `nonode`, and it ran one
/// of the two units it selected; every other run passes both.
const ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"[,}].*/\2/p')
if [ "$PROBE" = nonode ]; then
  printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"discovered":2,"selected":2,"completed":1},"checks":[{"id":"t1","verdict":"pass"},{"id":"node","verdict":"infra_failure","reason":"this machine cannot load the addon"}]}' "$commit" "$tree"
else
  printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"discovered":2,"selected":2,"completed":2},"checks":[{"id":"t1","verdict":"pass"},{"id":"node","verdict":"pass"}]}' "$commit" "$tree"
fi
"#;

/// A repository with the check, and its remote.
fn setup(test_box: &TestBox) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = vec![
        ("scripts/bay-check".to_owned(), ADAPTER.to_owned()),
        (".bay/policy.toml".to_owned(), POLICY.to_owned()),
    ];
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (repo, remote)
}

/// A runner's clone of `remote` named `name` on machine `machine`.
fn runner(test_box: &TestBox, remote: &Path, name: &str, machine: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    test_box.git_ok(&clone, &["config", "bay.machine", machine]);
    clone
}

/// One pass of the runner in `clone`, with `PROBE` set to `probe`.
fn pass(test_box: &TestBox, clone: &Path, probe: &str) -> Value {
    result_json(
        test_box
            .bay(clone)
            .args(["runner", "run", "--once", "--interval-s", "300"])
            .env("PROBE", probe),
    )
    .1
}

/// A change in a view of `repo`, submitted and left to the runners. Returns its id.
fn submitted(test_box: &TestBox, repo: &Path) -> String {
    let (id, view) = test_box.start_and_edit(repo);
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    id
}

/// The subjects of change `id`'s chain on `remote`, newest first.
fn chain(test_box: &TestBox, remote: &Path, id: &str) -> Vec<String> {
    test_box
        .git_bare(
            remote,
            &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

/// One pass of the runner in `clone`, with `PROBE` set to `probe`, on a clock that reads every
/// presence as gone.
fn pass_alone(test_box: &TestBox, clone: &Path, probe: &str) -> Value {
    result_json(
        test_box
            .bay(clone)
            .args(["runner", "run", "--once", "--interval-s", "300"])
            .env("PROBE", probe)
            .env("BAY_TEST_PRESENCE_CLOCK_SKEW_S", "100000"),
    )
    .1
}

/// How many subjects of change `id`'s chain start with `word`.
fn count(test_box: &TestBox, remote: &Path, id: &str, word: &str) -> usize {
    chain(test_box, remote, id)
        .iter()
        .filter(|subject| subject.starts_with(word))
        .count()
}

/// The claims of change `id` by `runner`.
fn claims_by(test_box: &TestBox, remote: &Path, id: &str, runner: &str) -> usize {
    chain(test_box, remote, id)
        .iter()
        .filter(|subject| {
            subject.starts_with("claim ") && subject.contains(&format!(" by {runner} "))
        })
        .count()
}

/// Moves main on `remote` by a commit made outside bay.
fn move_main(test_box: &TestBox, remote: &Path) {
    let outside = test_box.clone_of(remote, "outside");
    std::fs::write(outside.join("NOTES"), "main moved\n").unwrap();
    test_box.git_ok(&outside, &["add", "NOTES"]);
    test_box.git_ok(&outside, &["commit", "-q", "-m", "main moves"]);
    test_box.git_ok(&outside, &["push", "-q", "origin", "HEAD:main"]);
}

#[test]
fn a_runner_on_a_machine_that_could_not_run_it_leaves_it_once_its_wait_ended() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    let bad = runner(&test_box, &remote, "exec-a", "bad-box");
    let sibling = runner(&test_box, &remote, "exec-c", "bad-box");
    let good = runner(&test_box, &remote, "exec-b", "good-box");
    // GIVEN a change bad-box handed on while good-box was live
    pass(&test_box, &good, "ok");
    let id = submitted(&test_box, &repo);
    let first = pass(&test_box, &bad, "nonode");
    assert_eq!(
        first["detail"]["ran"][0]["reason"], "CHECK_UNAVAILABLE",
        "{first}"
    );
    // AND a runner on bad-box that read no other machine as live checked it there and blocked
    let blocked = pass_alone(&test_box, &sibling, "nonode");
    assert_eq!(
        count(&test_box, &remote, &id, "blocked"),
        1,
        "{blocked}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );
    let claims = claims_by(&test_box, &remote, &id, "exec-a");

    // WHEN main moves, so the change is asked again with no wait, and the runner on bad-box passes
    move_main(&test_box, &remote);
    let again = pass(&test_box, &bad, "nonode");

    // THEN it leaves the change to good-box: no claim of it, and no second block
    assert_eq!(
        claims_by(&test_box, &remote, &id, "exec-a"),
        claims,
        "{again}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );
    assert_eq!(count(&test_box, &remote, &id, "blocked"), 1, "{again}");
    // AND the runner on good-box lands it
    let landed = pass(&test_box, &good, "ok");
    assert!(
        test_box.trunk_change_ids(&remote).contains(&id),
        "the change did not land: {landed}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );
}

#[test]
fn a_machine_that_could_not_run_it_is_no_one_to_hand_it_back_to() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    let bad = runner(&test_box, &remote, "exec-a", "bad-box");
    let mid = runner(&test_box, &remote, "exec-m", "mid-box");
    // GIVEN a change bad-box handed on while a runner on mid-box was live
    pass(&test_box, &mid, "nonode");
    let id = submitted(&test_box, &repo);
    let first = pass(&test_box, &bad, "nonode");
    assert_eq!(
        first["detail"]["ran"][0]["reason"], "CHECK_UNAVAILABLE",
        "{first}"
    );

    // WHEN the check's unit node is an infra_failure on mid-box too
    let second = pass(&test_box, &mid, "nonode");

    // THEN the only other live runner is on bad-box, which could not run it, so it blocks there
    assert_eq!(
        second["detail"]["ran"][0]["reason"],
        "CHECK_UNKNOWN",
        "{second}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );
}

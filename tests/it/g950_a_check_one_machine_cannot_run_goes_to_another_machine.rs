//! G950 (a fleet finding, 2026-11-27: on one machine a check's unit could not load a native
//! addon, an `infra_failure`, while its other units passed; the run was `unknown` and named that
//! unit, so it was never handed on as G915 hands a check with no answer: the same machine ran it
//! again ten minutes later, and the change blocked `CHECK_UNKNOWN`). A gap whose units that did not
//! pass are all `infra_failure` is the machine's, and the operation goes to a live runner on
//! another machine. A runner on the same machine shares the cause: it is no one to hand it to, and
//! it passes the operation over while a runner on another machine is live.
//!
//! The adapter's unit `node` is an `infra_failure` when the declared `PROBE` variable says
//! `nonode`, and passes otherwise.
//!
//! Guards to mutate, in `src/publish.rs`: `machine_gap` refusing a row with reasons (the first
//! case blocks), `handed_to_runners` asking `live_runner_serving` for any other runner (the
//! third case hands it to its sibling), and `waits_for_worker` without `unable_here` (the
//! second case blocks on the sibling).

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

/// Asserts no subject of change `id`'s chain says it blocked.
fn never_blocked(test_box: &TestBox, remote: &Path, id: &str) {
    let subjects = chain(test_box, remote, id);
    assert!(
        !subjects
            .iter()
            .any(|subject| subject.starts_with("blocked")),
        "{subjects:#?}"
    );
}

#[test]
fn a_check_whose_only_gap_is_a_units_infra_failure_goes_to_another_machine() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    let bad = runner(&test_box, &remote, "exec-a", "bad-box");
    let good = runner(&test_box, &remote, "exec-b", "good-box");
    // GIVEN a live runner on good-box, and a change waiting on a quiet main
    pass(&test_box, &good, "ok");
    let id = submitted(&test_box, &repo);

    // WHEN the runner on bad-box takes it, and the check's unit node is an infra_failure there
    let first = pass(&test_box, &bad, "nonode");

    // THEN it hands the change on, and says which unit bad-box could not run
    assert_eq!(
        first["detail"]["ran"][0]["reason"],
        "CHECK_UNAVAILABLE",
        "{first}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );
    assert!(
        first["detail"]["ran"][0].to_string().contains(
            "this machine (bad-box) could not run it: its unit node is an infra_failure here"
        ),
        "{first}"
    );
    assert!(
        chain(&test_box, &remote, &id)
            .iter()
            .any(|subject| subject.contains("bad-box could not run")),
        "{:#?}",
        chain(&test_box, &remote, &id)
    );
    // AND the runner on good-box checks it again and lands it
    let second = pass(&test_box, &good, "ok");
    assert!(
        test_box.trunk_change_ids(&remote).contains(&id),
        "the change did not land: {second}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );
    never_blocked(&test_box, &remote, &id);
}

#[test]
fn a_runner_on_the_machine_that_could_not_run_it_leaves_it_to_another_machine() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    let bad = runner(&test_box, &remote, "exec-a", "bad-box");
    let sibling = runner(&test_box, &remote, "exec-c", "bad-box");
    let good = runner(&test_box, &remote, "exec-b", "good-box");
    // GIVEN live runners on both machines, and a change bad-box handed on
    pass(&test_box, &good, "ok");
    pass(&test_box, &sibling, "nonode");
    let id = submitted(&test_box, &repo);
    let first = pass(&test_box, &bad, "nonode");
    assert_eq!(
        first["detail"]["ran"][0]["reason"], "CHECK_UNAVAILABLE",
        "{first}"
    );

    // WHEN the other runner on bad-box passes
    let passed_over = pass(&test_box, &sibling, "nonode");

    // THEN it leaves the change, which neither landed nor blocked
    assert!(
        !test_box.trunk_change_ids(&remote).contains(&id),
        "{passed_over}"
    );
    never_blocked(&test_box, &remote, &id);
    // AND the runner on good-box lands it
    let landed = pass(&test_box, &good, "ok");
    assert!(
        test_box.trunk_change_ids(&remote).contains(&id),
        "the change did not land: {landed}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );
    never_blocked(&test_box, &remote, &id);
}

#[test]
fn with_live_runners_only_on_its_own_machine_the_pass_blocks_as_before() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    let bad = runner(&test_box, &remote, "exec-a", "bad-box");
    let sibling = runner(&test_box, &remote, "exec-c", "bad-box");
    // GIVEN a live runner on bad-box only, beside the one that takes the change
    pass(&test_box, &sibling, "nonode");
    let id = submitted(&test_box, &repo);

    // WHEN the check's unit node is an infra_failure on bad-box
    let first = pass(&test_box, &bad, "nonode");

    // THEN no runner on another machine can take it, so it blocks as before
    assert_eq!(
        first["detail"]["ran"][0]["reason"],
        "CHECK_UNKNOWN",
        "{first}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );
}

//! G951 (a fleet finding, 2026-11-27): on one machine, a post check had a unit that could not
//! load a native addon. That unit was an `infra_failure` on every run, and the check passed on
//! the other machines. The G822 election gave the check to a runner on that machine, so the
//! other runners left the check to it, and main stayed without an answer for hours. G950 does
//! not apply, because a post check holds no change.
//!
//! A machine whose two newest records of a post check on main's line say that it could not run
//! the check leaves the check to the other machines. It runs the check again when no runner on
//! another machine is live. The election does not give the check to a runner on that machine.
//!
//! The post check `suite` declares the variable `PROBE`. When `PROBE` is `nonode`, its unit
//! `node` is an `infra_failure`. Otherwise its one unit passes.
//!
//! Guards to mutate, in `src/bisect.rs`: `could_not_run` (make it always false; the first case
//! runs the check on bad-box), the two-record test in `unable_machines` (make it one record;
//! bad-box leaves the check after its first record), the `None` of `electors` (bad-box
//! runs the check beside good-box), the filter in `electors` (good-box leaves the check to the
//! bad-box owner), and the able runners that the test for another machine reads in
//! `electors` (read all live runners there, and the second case leaves the check to a runner on
//! the same machine).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::g822_a_post_check_runs_on_one_live_runner::{land, weight};
use support::{TestBox, check_files, passing_result, result_json};

/// The post check's result when `PROBE` is `nonode`: the unit `t1` passes and the unit
/// `node` is an `infra_failure`, so the check's verdict is unknown.
const NO_NODE: &str = r#"if [ "$1" = suite ] && [ "$PROBE" = nonode ]; then
  printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"discovered":2,"selected":2,"completed":1},"checks":[{"id":"t1","verdict":"pass"},{"id":"node","verdict":"infra_failure","reason":"this machine cannot load the addon"}]}' "$commit" "$tree"
  exit 0
fi"#;

/// A repository with the pre check `tests` and the post check `suite`, and its remote.
fn setup(test_box: &TestBox) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let mut files = check_files(NO_NODE, &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            let _ = write!(
                text,
                "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 600\nenv = [\"PROBE\"]\n"
            );
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\nbisect_post = false\n".to_owned(),
    ));
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

/// The two names, the one that weighs more with `suite` first: that runner owns the check.
fn by_weight<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if weight(a) > weight(b) {
        (a, b)
    } else {
        (b, a)
    }
}

/// One pass of the runner in `clone` with `PROBE` set to `probe`: the post check it ran, or
/// `""`.
fn post(test_box: &TestBox, clone: &Path, probe: &str) -> String {
    let (code, ran) = result_json(
        test_box
            .bay(clone)
            .args(["runner", "run", "--once"])
            .env("PROBE", probe),
    );
    assert_eq!(code, 0, "{ran}");
    ran["detail"]["ran"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|row| row.get("post"))
        .and_then(|post| post[0]["check"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[test]
fn a_machine_that_could_not_run_a_post_check_twice_leaves_it_to_another_machine() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    let (owner, other) = by_weight("exec-a", "exec-b");
    let bad = runner(&test_box, &remote, owner, "bad-box");
    let good = runner(&test_box, &remote, other, "good-box");

    // GIVEN the owner on bad-box, where the unit node is an infra_failure, and a live runner
    // on good-box that ran the check once
    land(&test_box, &repo, "src", "src/s0.rs");
    assert_eq!(post(&test_box, &bad, "nonode"), "suite");
    assert_eq!(post(&test_box, &good, "ok"), "suite");
    // AND one record that bad-box could not run the check does not move it
    land(&test_box, &repo, "src", "src/s1.rs");
    assert_eq!(
        post(&test_box, &bad, "nonode"),
        "suite",
        "one record moved the check off its machine"
    );

    // WHEN a new tip lands after the second such record
    land(&test_box, &repo, "src", "src/s2.rs");

    // THEN the runner on bad-box leaves the check
    assert_eq!(
        post(&test_box, &bad, "nonode"),
        "",
        "a machine that could not run the check ran it again while another machine was live"
    );
    // AND the runner on good-box runs it
    assert_eq!(
        post(&test_box, &good, "ok"),
        "suite",
        "the check waited on an owner whose machine could not run it"
    );
}

#[test]
fn with_live_runners_only_on_its_own_machine_it_still_runs_the_check() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    let (owner, other) = by_weight("exec-a", "exec-c");
    let bad = runner(&test_box, &remote, owner, "bad-box");
    let sibling = runner(&test_box, &remote, other, "bad-box");

    // GIVEN two records that bad-box could not run the check, and a live runner on bad-box only
    land(&test_box, &repo, "src", "src/s0.rs");
    assert_eq!(post(&test_box, &bad, "nonode"), "suite");
    assert_eq!(post(&test_box, &sibling, "nonode"), "suite");

    // WHEN new tips land
    // THEN the owner runs the check on each, because no runner on another machine can take it
    for (n, path) in ["src/s1.rs", "src/s2.rs"].into_iter().enumerate() {
        land(&test_box, &repo, "src", path);
        assert_eq!(
            post(&test_box, &bad, "nonode"),
            "suite",
            "tip {n}: the check was left to a runner on the same machine"
        );
    }
}

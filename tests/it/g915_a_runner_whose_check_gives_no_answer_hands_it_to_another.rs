//! G915 (a fleet finding, 2026-11-25: a runner's check gave no answer in 0.3 s on its machine,
//! and the change was blocked `CHECK_UNKNOWN` on a quiet main: bay asks such a change again only
//! when main moves, and its author's unchanged `bay merge` is refused). A runner's pass whose
//! check gives no answer on its machine hands the operation to another live runner that serves
//! the check's environment, keeping its place, once per machine, as an own run does (G811): the
//! other runner checks it again (unknown never passes) and lands it. With no other live runner it
//! blocks as before, and a blocked `CHECK_UNKNOWN` names the machine the check ran on.
//!
//! The adapter gives no answer when its declared `PROBE` variable says `silent` and passes
//! otherwise.
//!
//! Guard to mutate, in `src/publish.rs` `handed_to_runners`: answering `None` for a runner's
//! pass (`ctx.visit != Visit::Own`): the pass blocks `CHECK_UNKNOWN`.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// A repository whose one check declares `PROBE` and gives no answer when it is `silent`, its
/// machine named `dev-box`. Returns the repository and the remote.
fn setup(test_box: &TestBox) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files: Vec<(String, String)> = check_files(
        "[ \"$PROBE\" = silent ] && exit 0",
        &passing_result(1),
        1,
        60,
    )
    .into_iter()
    .map(|(path, text)| {
        if path == ".bay/policy.toml" {
            (path, format!("{text}env = [\"PROBE\"]\n"))
        } else {
            (path, text)
        }
    })
    .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    test_box.git_ok(&repo, &["config", "bay.machine", "dev-box"]);
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

#[test]
fn a_runner_whose_check_gives_no_answer_hands_it_to_another_live_runner() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    let bad = runner(&test_box, &remote, "exec-a", "bad-box");
    let good = runner(&test_box, &remote, "exec-b", "good-box");
    // GIVEN a live runner on good-box, and a change waiting on a quiet main
    pass(&test_box, &good, "ok");
    let id = submitted(&test_box, &repo);

    // WHEN the runner on bad-box takes it and its check gives no answer there
    let first = pass(&test_box, &bad, "silent");

    // THEN it hands the change on, keeping its place, and nothing blocks it
    assert_eq!(
        first["detail"]["ran"][0]["reason"],
        "CHECK_UNAVAILABLE",
        "{first}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );
    assert!(
        chain(&test_box, &remote, &id)
            .iter()
            .any(|subject| subject.contains("bad-box could not run")),
        "{:#?}",
        chain(&test_box, &remote, &id)
    );
    // AND the runner on good-box checks it again and lands it, with no new request
    let second = pass(&test_box, &good, "ok");
    assert!(
        test_box.trunk_change_ids(&remote).contains(&id),
        "the change did not land: {second}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );
    let subjects = chain(&test_box, &remote, &id);
    assert!(
        !subjects
            .iter()
            .any(|subject| subject.starts_with("blocked")),
        "{subjects:#?}"
    );
}

#[test]
fn with_no_other_live_runner_the_pass_blocks_as_before() {
    let test_box = TestBox::new();
    let (repo, remote) = setup(&test_box);
    let bad = runner(&test_box, &remote, "exec-a", "bad-box");
    let id = submitted(&test_box, &repo);
    let first = pass(&test_box, &bad, "silent");
    assert_eq!(
        first["detail"]["ran"][0]["reason"],
        "CHECK_UNKNOWN",
        "{first}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );
}

#[test]
fn a_blocked_check_with_no_answer_names_the_machine_it_ran_on() {
    let test_box = TestBox::new();
    let (repo, _remote) = setup(&test_box);
    let (_id, view) = test_box.start_and_edit(&repo);
    let (_, result) = result_json(
        test_box
            .bay(&view)
            .args(["merge", "--wait=180"])
            .env("PROBE", "silent"),
    );
    assert_eq!(result["reason"], "CHECK_UNKNOWN", "{result}");
    assert_eq!(result["detail"]["checked_on"], "dev-box", "{result}");
    assert!(
        result["say"]
            .as_str()
            .unwrap_or_default()
            .contains("no clear answer on dev-box"),
        "{result}"
    );
}

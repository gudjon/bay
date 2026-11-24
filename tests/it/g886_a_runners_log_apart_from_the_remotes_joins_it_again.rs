//! G886 (a fleet measurement, 2026-11-24: the own logs of 3 of 5 runners had forked from the
//! remote's, each at a batch's landing whose push the remote took while its answer said refused;
//! every later push of the log was refused, every landing's publishing push with it, and no other
//! box held the records written since the fork). A refused push of a runner's own log joins it to
//! the remote's: never forced, no record lost, none twice.
//!
//! G887 (the same measurement: a landing fetched the logs twice for each event that every landing
//! since had held): a pass whose blocked change's event a log on this box holds fetches no log to
//! look for it.
//!
//! Guards to mutate: in `src/log.rs` `push`, drop the call of `reconcile_own` (the logs stay
//! apart); in `rejoined`, drop the test of `trees` (the remote's record goes twice); in
//! `src/publish.rs` `recover_events`, fetch the logs before the first read of `held`.

#![cfg(unix)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

const LOG: &str = "refs/bay/log/local";

/// The tree of a log record whose `record.json` is `json`, written in the store at `dir`.
fn record_tree(test_box: &TestBox, dir: &Path, json: &str) -> String {
    let piped = |args: &[&str], input: &str| {
        let mut child = test_box
            .git(dir)
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
    let blob = piped(&["hash-object", "-w", "--stdin"], json);
    piped(&["mktree"], &format!("100644 blob {blob}\trecord.json\n"))
}

/// A log commit of `tree` on `parent`, made at `when` (seconds), in the store at `dir`: the
/// same record made at two times is two commits, as on a runner whose log forked.
fn log_commit(
    test_box: &TestBox,
    dir: &Path,
    (when, tree, parent): (u64, &str, &str),
    subject: &str,
) -> String {
    let when = format!("{when} +0000");
    let out = test_box
        .git(dir)
        .args(["commit-tree", tree, "-p", parent, "-m", subject])
        .env("GIT_AUTHOR_DATE", &when)
        .env("GIT_COMMITTER_DATE", &when)
        .output()
        .unwrap();
    assert!(out.status.success(), "commit-tree");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// How many commits of the log in the store at `dir` have `subject`.
fn held(test_box: &TestBox, dir: &Path, subject: &str) -> usize {
    test_box
        .git_ok(dir, &["log", "--format=%s", LOG])
        .lines()
        .filter(|line| *line == subject)
        .count()
}

/// A runner that has visited one change whose check said `verdict`: the remote and the runner's
/// clone.
fn visited(test_box: &TestBox, verdict: &str) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let result = passing_result(1).replace("\\\"pass\\\"", verdict);
    let files = check_files("", &result, 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let executor = test_box.clone_of(&remote, "executor");
    test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    (remote, executor)
}

#[test]
fn a_runners_log_apart_from_the_remotes_joins_it_again() {
    let test_box = TestBox::new();
    let (remote, executor) = visited(&test_box, "\\\"pass\\\"");
    let pushed = test_box.git_ok(&executor, &["rev-parse", LOG]);
    assert_eq!(test_box.git_ok(&remote, &["rev-parse", LOG]), pushed);
    let tree = test_box.git_ok(&executor, &["rev-parse", &format!("{pushed}^{{tree}}")]);

    // GIVEN a record the remote took from a push whose answer said refused, so this box's log
    // never took it
    let landed = "event check_failed evt_landed";
    let theirs = log_commit(&test_box, &remote, (1_700_000_000, &tree, &pushed), landed);
    test_box.git_ok(&remote, &["update-ref", LOG, &theirs, &pushed]);
    // AND the same record written again on this box's log, and a record of its own after it
    let again = log_commit(
        &test_box,
        &executor,
        (1_700_000_100, &tree, &pushed),
        landed,
    );
    assert_ne!(again, theirs);
    let own = "event check_failed evt_own";
    let own_tree = record_tree(
        &test_box,
        &executor,
        "{\"schema\":\"bay.event/1\",\"id\":\"evt_own\",\"kind\":\"check_failed\"}\n",
    );
    let mine = log_commit(
        &test_box,
        &executor,
        (1_700_000_200, &own_tree, &again),
        own,
    );
    test_box.git_ok(&executor, &["update-ref", LOG, &mine, &pushed]);

    // WHEN a pass pushes the log
    test_box.bay_ok(&executor, &["runner", "run", "--once"]);

    // THEN the two logs are one line, the remote's never forced, each record on it once
    let tip = test_box.git_ok(&executor, &["rev-parse", LOG]);
    assert_eq!(
        test_box.git_ok(&remote, &["rev-parse", LOG]),
        tip,
        "the runner's log and the remote's are still apart"
    );
    test_box.git_ok(&remote, &["merge-base", "--is-ancestor", &theirs, LOG]);
    assert_eq!(held(&test_box, &remote, landed), 1, "the landed record");
    assert_eq!(held(&test_box, &remote, own), 1, "the runner's own record");
}

/// Each Git call one command made, its arguments read from its trace, in order.
fn traced_calls(trace: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(trace)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let call: serde_json::Value = serde_json::from_str(line).ok()?;
            let args = call["args"].as_array()?;
            Some(
                args.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn a_pass_fetches_no_log_for_an_event_a_log_here_holds() {
    let test_box = TestBox::new();
    // GIVEN a change its check refused, whose event the runner's log holds
    let (_, executor) = visited(&test_box, "\\\"product_failure\\\"");
    let events = test_box.git_ok(&executor, &["log", "--format=%s", LOG]);
    assert!(
        events.lines().any(|subject| subject.starts_with("event ")),
        "no event on the log: {events}"
    );

    // WHEN a pass runs
    let trace = test_box.path().join("pass.jsonl");
    let mut pass = test_box.bay(&executor);
    pass.env("BAY_TEST_GIT_TRACE", &trace);
    let (code, ran) = result_json(pass.args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{ran}");

    // THEN no log is fetched to look for it
    let fetched: Vec<_> = traced_calls(&trace)
        .into_iter()
        .filter(|args| {
            args.iter().any(|arg| arg == "fetch")
                && args.iter().any(|arg| arg.contains("refs/bay/log/"))
        })
        .collect();
    assert!(fetched.is_empty(), "the logs fetched: {fetched:?}");
}

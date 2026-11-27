//! G931 (a fleet finding, 2026-11-27: one landing batch of six changes left every member's chain
//! saying landed, while main never held the batch and the remote held no landed marker of any
//! member. Each change kept its intent: a new change for it was refused `CHANGE_SUPERSEDED`, and
//! the old one answered `MERGE_OUTCOME_UNKNOWN` for ever).
//!
//! The author's `bay merge` now opens such a change again, once its landing record is older than
//! a push under way could be, and the change lands like any other. A younger record is still
//! answered `MERGE_OUTCOME_UNKNOWN`, with the seconds left before it opens again.
//!
//! Guards to mutate: in `src/publish.rs`, `landed_if_on_main` reopens only past
//! `REOPEN_AFTER_S` (drop the age test, and the young phantom opens again under a push that may
//! still land it); it ends the landing's operation failed and sets the change ready (keep the
//! operation published, and the land answers `CHANGE_ALREADY_MERGED` again).
//!
//! G952 (a fleet finding, 2026-11-27, change 4ked73uh: `bay status` said such a change "is on
//! main", its receipt said it would recover by itself, and the age was read from the chain's
//! head, which each answer's receipt moves). The age is the chain's newest state that is not a
//! receipt (`recorded_at`: read the head again, and a receipt puts the reopen off); `merge
//! --wait` waits out `reopens_in_s` (`reopens_in` in `wait_land`); the say, the receipt's
//! recovery and the status sentence name the author's next `bay merge` (`Reason::say`,
//! `receipts::from_result`, `change_state_say`).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use serde_json::Value;

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

/// Runs git in `dir` with `input` on its stdin: its stdout, trimmed.
fn git_with_input(test_box: &TestBox, dir: &Path, args: &[&str], input: &str) -> String {
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
}

/// Appends a state to the remote chain of change `id`, committed now, with the subject a
/// receipt's append gives it: what each `MERGE_OUTCOME_UNKNOWN` answer writes.
fn receipt_now(test_box: &TestBox, remote: &Path, id: &str) {
    let writer = test_box.clone_of(remote, "receipt-writer");
    let name = format!("refs/bay/changes/{id}");
    let git = |args: &[&str]| test_box.git_ok(&writer, args);
    git(&["fetch", "-q", "origin", &format!("+{name}:{name}")]);
    let head = git(&["rev-parse", &name]);
    let tree = git(&["rev-parse", &format!("{head}^{{tree}}")]);
    let draft = git(&["rev-parse", &format!("{head}^2")]);
    let commit = git(&[
        "commit-tree",
        &tree,
        "-p",
        &head,
        "-p",
        &draft,
        "-m",
        "receipt r-g952",
    ]);
    git(&["push", "-q", "origin", &format!("{commit}:{name}")]);
}

/// A change queued with one passing check, whose runner ended before its publishing push with
/// its checked record on the remote; then the remote chain of the change says it landed, as the
/// split batch left it, its record committed `age_s` seconds ago. Main never holds it.
fn phantom(age_s: u64) -> (TestBox, std::path::PathBuf, String, std::path::PathBuf) {
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
    let e1 = test_box.clone_of(&remote, "e1");
    test_box.git_ok(&e1, &["config", "bay.producerName", "e1-checks"]);
    let out = test_box
        .bay_process(&e1)
        .env("BAY_TEST_LAND_APART", "1")
        .env("BAY_TEST_KILL_AT", "before-publish")
        .args(["--json", "runner", "run", "--once"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(137), "{out:?}");

    let forger = test_box.clone_of(&remote, "forger");
    let name = format!("refs/bay/changes/{id}");
    test_box.git_ok(
        &forger,
        &["fetch", "-q", "origin", &format!("+{name}:{name}")],
    );
    let git = |args: &[&str]| test_box.git_ok(&forger, args);
    let head = git(&["rev-parse", &name]);
    let draft = git(&["rev-parse", &format!("{head}^2")]);
    let mut record = chain(&git, &id);
    record["state"] = Value::from("landed");
    let op = record["operations"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    op["state"] = Value::from("published");
    op["outcome"] = Value::Null;
    let candidate = op["attempts"].as_array().unwrap().last().unwrap()["commit"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut json = serde_json::to_string_pretty(&record).unwrap();
    json.push('\n');
    let blob = git_with_input(&test_box, &forger, &["hash-object", "-w", "--stdin"], &json);
    let tree = git_with_input(
        &test_box,
        &forger,
        &["mktree"],
        &format!("100644 blob {blob}\tchange.json\n"),
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let out = test_box
        .git(&forger)
        .env("GIT_COMMITTER_DATE", format!("@{} +0000", now - age_s))
        .args([
            "commit-tree",
            &tree,
            "-p",
            &head,
            "-p",
            &draft,
            "-p",
            &candidate,
        ])
        .args(["-m", &format!("landed {id}.1")])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let commit = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    git(&["push", "-q", "origin", &format!("{commit}:{name}")]);
    assert_eq!(on_main(&test_box, &remote, &id), 0);
    (test_box, remote, id, view)
}

#[test]
fn a_landing_main_never_took_opens_its_change_again_and_it_lands_once() {
    // GIVEN a change whose chain says it landed an hour ago, which main never took
    let (test_box, remote, id, view) = phantom(3600);
    let remote_git = |args: &[&str]| test_box.git_bare(&remote, args);

    // WHEN its author merges it
    let (code, merged) = result_json(test_box.bay(&view).arg("merge"));

    // THEN it is not answered merged: the change is ready again, its landing failed, and a
    // new operation is in line
    assert_eq!(code, 0, "{merged}");
    assert_ne!(merged["decision"], "merged", "{merged}");
    let record = chain(&remote_git, &id);
    assert_eq!(record["state"], "ready", "{record}");
    let ops = record["operations"].as_array().unwrap();
    assert_eq!(ops[0]["state"], "failed", "{record}");
    assert_eq!(ops[0]["outcome"], "MERGE_OUTCOME_UNKNOWN", "{record}");
    assert_eq!(ops.len(), 2, "{record}");

    // AND a runner lands it, once
    let e2 = test_box.clone_of(&remote, "e2");
    test_box.git_ok(&e2, &["config", "bay.producerName", "e2-checks"]);
    for _ in 0..4 {
        let (code, ran) = result_json(test_box.bay(&e2).args(["runner", "run", "--once"]));
        assert_eq!(code, 0, "{ran}");
        if on_main(&test_box, &remote, &id) > 0 {
            break;
        }
    }
    assert_eq!(on_main(&test_box, &remote, &id), 1);
    assert_eq!(chain(&remote_git, &id)["state"], "landed");
}

#[test]
fn a_young_landing_main_lacks_is_answered_unknown_and_left_as_it_is() {
    // GIVEN a change whose chain said it landed a moment ago, which main does not hold yet
    let (test_box, remote, id, view) = phantom(0);
    let remote_git = |args: &[&str]| test_box.git_bare(&remote, args);

    // WHEN its author merges it
    let (_, merged) = result_json(test_box.bay(&view).arg("merge"));

    // THEN the outcome is unknown, with the time before it opens again, and nothing is written
    assert_eq!(merged["reason"], "MERGE_OUTCOME_UNKNOWN", "{merged}");
    let left = merged["detail"]["reopens_in_s"].as_u64().unwrap_or(0);
    assert!(left > 0 && left <= 600, "{merged}");
    assert_eq!(chain(&remote_git, &id)["state"], "landed");

    // AND (G952) it says when to merge it again, and status never says it is on main
    let say = merged["say"].as_str().unwrap_or_default();
    assert!(say.contains("again in"), "{merged}");
    let status = test_box.bay_ok(&view, &["status", &id]);
    let said = status["say"].as_str().unwrap_or_default();
    assert!(said.contains("does not hold it"), "{status}");
    let receipts = status["detail"]["changes"][0]["receipts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        receipts.iter().any(|receipt| receipt["recovery"]
            .as_str()
            .is_some_and(|recovery| recovery.starts_with("run bay merge again in"))),
        "{status}"
    );
}

#[test]
fn a_receipt_written_since_the_landing_never_puts_its_reopen_off() {
    // GIVEN a change whose chain said it landed an hour ago, which main never took, and a
    // receipt an answer wrote on its chain a moment ago
    let (test_box, remote, id, view) = phantom(3600);
    receipt_now(&test_box, &remote, &id);
    let remote_git = |args: &[&str]| test_box.git_bare(&remote, args);

    // WHEN its author merges it
    let (code, merged) = result_json(test_box.bay(&view).arg("merge"));

    // THEN it opens again: the receipt is no record of the landing
    assert_eq!(code, 0, "{merged}");
    assert_ne!(merged["reason"], "MERGE_OUTCOME_UNKNOWN", "{merged}");
    assert_eq!(chain(&remote_git, &id)["state"], "ready", "{merged}");
}

#[test]
fn a_waiting_merge_waits_out_the_reopen_and_lands_the_change_once() {
    // GIVEN a change whose chain said it landed ten seconds short of the reopen, which main
    // never took
    let (test_box, remote, id, view) = phantom(590);

    // WHEN its author merges it and waits
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    // THEN the wait outlasts the reopen and the change lands, once
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    assert_eq!(on_main(&test_box, &remote, &id), 1);
}

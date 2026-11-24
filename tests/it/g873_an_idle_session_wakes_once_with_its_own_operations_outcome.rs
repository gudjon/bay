//! G873: an idle session wakes once with the outcome of each operation it left queued, so it
//! never reads `bay status` to learn it (BACKLOG 253 b).
//!
//! A `bay merge` a session runs that answers `queued` leaves a marker for the operation; any
//! later answer the session reads for that operation (a `merge --wait`, say) removes it. The idle
//! wait (`bay hook talk-wait`, an asynchronous Stop hook) reads the marked operations' chains from
//! the remote and wakes the session (exit 2) with one line for each one that ended: merged, once
//! main holds it (G875), or refused, with its reason. The line removes the marker, so a second
//! wait has nothing to say, and with no talk policy it ends at once. An operation whose change
//! has landed is merged, whatever state its own record was left in (G880).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

const SESSION: &str = "s-g873-seat";

/// `bay hook talk-wait` in `dir` as the session: its exit code and its stderr, the wake's text.
fn wait(test_box: &TestBox, dir: &std::path::Path) -> (i32, String, Duration) {
    let began = Instant::now();
    let out = test_box
        .bay(dir)
        .env("BAY_SESSION", SESSION)
        .env("BAY_TEST_TALK_POLL_MS", "20")
        .args(["hook", "talk-wait"])
        .write_stdin(format!(
            "{{\"session_id\": \"{SESSION}\", \"hook_event_name\": \"Stop\"}}"
        ))
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        began.elapsed(),
    )
}

/// A repository whose one check passes (or, with `fail`, fails), and a change of the session's
/// queued by `bay merge`: the change, the repository, the remote and the change's view.
fn queued(test_box: &TestBox, fail: bool) -> (String, PathBuf, PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let result = if fail {
        passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"")
    } else {
        passing_result(1)
    };
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &result, 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).env("BAY_SESSION", SESSION).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");
    (id, repo, remote, view)
}

#[test]
fn a_merged_operation_wakes_the_session_once() {
    let test_box = TestBox::new();
    let (id, repo, remote, _) = queued(&test_box, false);
    // GIVEN a runner on another clone merged the session's queued operation
    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
    // WHEN the session goes idle
    let (code, told, _) = wait(&test_box, &repo);
    // THEN it wakes with one line that says the operation merged
    assert_eq!(code, 2, "{told}");
    assert!(
        told.contains(&format!("bay: change {id}: merged {id}.1 as ")),
        "{told}"
    );
    assert_eq!(told.lines().count(), 1, "{told}");
    // AND a second idle wait has nothing new to say, and ends at once
    let (code, again, took) = wait(&test_box, &repo);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again, "", "a second wake with no new outcome");
    assert!(took < Duration::from_secs(20), "{took:?}");
}

/// `git <args>` in `dir` with `input` on stdin: its trimmed stdout.
fn git_in(test_box: &TestBox, dir: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = test_box
        .git(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// One more state commit of the chain of `id` in `dir`, pushed to its remote: the change stays
/// `landed` and its operation reads `admitted` again, `QUEUED`, as a fleet's record of an
/// operation a batch merged read (BACKLOG 213, 238). The repository has no key registry, so its
/// chain is unsigned.
fn leave_admitted(test_box: &TestBox, dir: &Path, id: &str) {
    let name = format!("refs/bay/changes/{id}");
    let head = test_box.git_ok(dir, &["rev-parse", &name]);
    let text = test_box.git_ok(dir, &["cat-file", "blob", &format!("{head}:change.json")]);
    let mut record: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(record["state"], "landed", "{record}");
    let op = record["operations"]
        .as_array_mut()
        .and_then(|ops| ops.last_mut())
        .expect("the change has an operation");
    op["state"] = Value::from("admitted");
    op["outcome"] = Value::from("QUEUED");
    let blob = git_in(
        test_box,
        dir,
        &["hash-object", "-w", "--stdin"],
        record.to_string().as_bytes(),
    );
    let entries: String = test_box
        .git_ok(dir, &["ls-tree", &head])
        .lines()
        .map(|line| {
            if line.ends_with("\tchange.json") {
                format!("100644 blob {blob}\tchange.json\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    let tree = git_in(test_box, dir, &["mktree"], entries.as_bytes());
    let parents = test_box.git_ok(dir, &["show", "-s", "--format=%P", &head]);
    let mut args = vec![
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit-tree",
        &tree,
        "-p",
        &head,
    ];
    for parent in parents.split_whitespace().skip(1) {
        args.extend(["-p", parent]);
    }
    args.extend(["--no-gpg-sign", "-m", "admitted again"]);
    let commit = test_box.git_ok(dir, &args);
    test_box.git_ok(dir, &["update-ref", &name, &commit, &head]);
    test_box.git_ok(dir, &["push", "-q", "origin", &name]);
}

/// G880: the record of a merged operation left at `admitted` told the session it still waited,
/// at the end of every wait, though its change had landed.
#[test]
fn a_merged_operation_whose_record_reads_admitted_wakes_the_session_once_as_merged() {
    let test_box = TestBox::new();
    let (id, repo, remote, _) = queued(&test_box, false);
    // GIVEN a runner merged the session's operation, and its record then read `admitted` again
    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
    leave_admitted(&test_box, &executor, &id);
    // WHEN the session goes idle
    let (code, told, _) = wait(&test_box, &repo);
    // THEN it wakes once with the merge, never with a line that it still waits
    assert_eq!(code, 2, "{told}");
    assert!(
        told.contains(&format!("bay: change {id}: merged {id}.1 as ")),
        "{told}"
    );
    assert!(!told.contains("still"), "{told}");
    assert_eq!(told.lines().count(), 1, "{told}");
    // AND a second idle wait has nothing new to say, and ends at once
    let (code, again, took) = wait(&test_box, &repo);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again, "", "a second wake with no new outcome");
    assert!(took < Duration::from_secs(20), "{took:?}");
}

#[test]
fn a_refused_operation_wakes_the_session_once_with_its_reason() {
    let test_box = TestBox::new();
    let (id, repo, remote, _) = queued(&test_box, true);
    // GIVEN a runner refused the session's queued operation: its check failed
    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["reason"], "CHECK_FAILED", "{ran}");
    // WHEN the session goes idle
    let (code, told, _) = wait(&test_box, &repo);
    // THEN it wakes with one line that names the reason and what to do
    assert_eq!(code, 2, "{told}");
    assert!(
        told.contains(&format!(
            "bay: change {id}: refused CHECK_FAILED on {id}.1: allowed "
        )),
        "{told}"
    );
    assert_eq!(told.lines().count(), 1, "{told}");
    // AND a second idle wait has nothing new to say
    let (code, again, _) = wait(&test_box, &repo);
    assert_eq!(code, 0, "{again}");
    assert_eq!(again, "", "a second wake with no new outcome");
}

#[test]
fn an_outcome_the_session_read_itself_wakes_nothing() {
    let test_box = TestBox::new();
    let (_, repo, _, view) = queued(&test_box, false);
    // GIVEN the session ran the operation to its outcome itself
    let (code, merged) = result_json(
        test_box
            .bay(&view)
            .env("BAY_SESSION", SESSION)
            .args(["merge", "--wait"]),
    );
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    // WHEN the session goes idle
    let (code, told, took) = wait(&test_box, &repo);
    // THEN nothing wakes it, and the wait ends at once
    assert_eq!(code, 0, "{told}");
    assert_eq!(told, "");
    assert!(took < Duration::from_secs(20), "{took:?}");
}

#[test]
fn a_persons_merge_leaves_no_wait() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (_, view) = test_box.start_and_edit(&repo);
    // GIVEN a merge no agent's session ran (no BAY_SESSION, no hook named its process)
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    assert_eq!(queued["decision"], "queued", "{queued}");
    // THEN no marker is left for any wait
    let awaiting = repo.join(".git/bay/awaiting");
    let marked = std::fs::read_dir(&awaiting).map_or(0, |dir| {
        dir.flatten()
            .flat_map(|actor| {
                std::fs::read_dir(actor.path())
                    .into_iter()
                    .flatten()
                    .flatten()
            })
            .count()
    });
    assert_eq!(marked, 0, "a marker under {}", awaiting.display());
}

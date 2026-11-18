//! G776 (a measurement, 2026-11-17: 61 agent sessions read `bay status` 12,452 times in a day, a
//! median of 315 each and 1,090 inside a shell loop, each read a turn of the model). Waiting is
//! cheaper than polling:
//! - a status read that finds what the session's earlier read of it found says so, and names the
//!   wait in one call (`detail.unchanged_s`, `detail.wait_with`);
//! - a shell call that loops over `bay status` hears once an hour that a wait in one call, run as
//!   a background task, replaces it;
//! - `bay merge --wait=<s>` bounds the whole command, its fetch and submit too (a `--wait=90`
//!   returned after 151 s), and its sentence names the next step when the change is still in line;
//! - a change held until another merges (`--after`) names that change and where it stands, and
//!   the line's longest wait counts only changes in line.
//!
//! Guards to mutate, one per case: in `src/status.rs` `status`, drop the call of `unchanged`; in
//! `src/agent_hook.rs` `pre_tool`, drop the `poll_notice` branch; in `src/publish.rs`
//! `observe_land`, time the watch from after the submit (`until` = now + `timeout` there); in
//! `src/status.rs` `describe`, drop the `after_here` line.

use crate::support;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use serde_json::{Value, json};
use support::{TestBox, check_files, passing_result, result_json, unchecked_files};

/// A store whose changes wait for a runner: a check covers `src/`.
fn checked_store(test_box: &TestBox) -> PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    repo
}

/// Starts `intent` with `extra` arguments and writes a file in its view: its id and view.
fn start_writing(
    test_box: &TestBox,
    repo: &Path,
    intent: &str,
    extra: &[&str],
) -> (String, PathBuf) {
    let mut args = vec!["start", intent, "--paths", "src/"];
    args.extend(extra);
    let started = test_box.bay_ok(repo, &args);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(format!("src/{id}.rs")), "fn x() {}\n").unwrap();
    (id, view)
}

#[test]
fn a_read_that_finds_nothing_new_says_so_and_names_the_wait() {
    let test_box = TestBox::new();
    let repo = checked_store(&test_box);
    let (id, view) = start_writing(&test_box, &repo, "CARD-1", &[]);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge"]));
    assert_eq!(code, 0, "{merged}");
    let read = |session: &str| {
        let (code, read) = result_json(
            test_box
                .bay(&repo)
                .args(["status", &id])
                .env("BAY_SESSION", session),
        );
        assert_eq!(code, 0, "{read}");
        read
    };

    // GIVEN a session read the change once
    let first = read("s-g776-one");
    assert!(first["detail"]["unchanged_s"].is_null(), "{first}");

    // WHEN it reads it again with nothing moved
    let again = read("s-g776-one");

    // THEN the read says nothing changed, and names the wait in one call
    assert!(again["detail"]["unchanged_s"].is_u64(), "{again}");
    assert_eq!(again["detail"]["wait_with"], id.as_str(), "{again}");
    let say = again["say"].as_str().unwrap();
    assert!(say.contains("Nothing changed since your read"), "{say}");
    assert!(say.contains(&format!("`bay merge {id} --wait`")), "{say}");
    // AND another session's first read is not told so
    let other = read("s-g776-two");
    assert!(other["detail"]["unchanged_s"].is_null(), "{other}");
}

/// `bay hook tool.before` with `payload` on stdin: its exit, stdout and stderr.
fn hook(test_box: &TestBox, cwd: &Path, payload: &Value) -> (Option<i32>, String, String) {
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", "tool.before"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn bash(session: &str, cwd: &Path, command: &str) -> Value {
    json!({
        "session_id": session,
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": cwd,
    })
}

#[test]
fn a_loop_over_status_hears_once_that_one_wait_replaces_it() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let looped = "until bay status abcdefgh --json | grep -q merged; do sleep 20; done";

    // WHEN a session's shell call loops over `bay status`
    let (exit, stdout, stderr) = hook(&test_box, &repo, &bash("s-poll", &repo, looped));

    // THEN it runs, with one line naming the wait in one call as a background task
    assert_eq!(exit, Some(0), "{stderr}");
    assert!(stdout.contains("background task"), "{stdout}");
    assert!(stderr.contains("`bay merge <change> --wait`"), "{stderr}");
    // AND the next loop within the hour is not told again
    let (_, again, _) = hook(&test_box, &repo, &bash("s-poll", &repo, looped));
    assert!(!again.contains("background task"), "{again}");
    // AND a single read is never told
    let (_, once, _) = hook(
        &test_box,
        &repo,
        &bash("s-other", &repo, "bay status --json"),
    );
    assert!(!once.contains("background task"), "{once}");
}

#[test]
fn a_wait_is_bounded_from_the_command_and_names_the_next_step() {
    let test_box = TestBox::new();
    let repo = checked_store(&test_box);
    let (id, view) = start_writing(&test_box, &repo, "CARD-1", &[]);
    // A slow fetch and submit: 14 s, against a wait of 12 s.
    let (delay_s, wait_s) = (14, 12);

    // WHEN `bay merge --wait=12` runs where nothing runs the check
    let began = Instant::now();
    let (code, waited) = result_json(
        test_box
            .bay(&view)
            .args(["merge", &format!("--wait={wait_s}")])
            .env("BAY_TEST_LAND_DELAY_MS", (delay_s * 1000).to_string()),
    );
    let took = began.elapsed();

    // THEN its wait was spent from the command's start: past it once submitted, it watched no
    // more (it would watch the whole 12 s if timed from after the submit)
    let watched = waited["detail"]["watched_s"].as_u64().unwrap_or(u64::MAX);
    assert!(
        watched <= 1,
        "--wait={wait_s} watched {watched} s after a {delay_s} s submit ({took:?} in all): {waited}"
    );
    // AND its sentence names the next step, a wait again in one call
    assert!(code == 0 || code == 2, "{waited}");
    let say = waited["say"].as_str().unwrap();
    assert!(
        say.contains(&format!("`bay merge {id} --wait` waits again in one call")),
        "{say}"
    );
}

#[test]
fn a_change_held_by_another_names_it_and_its_wait_is_not_the_lines() {
    let test_box = TestBox::new();
    let repo = checked_store(&test_box);
    // GIVEN a change B that merges only after A, which is not submitted
    let (a, _) = start_writing(&test_box, &repo, "CARD-A", &[]);
    let (b, b_view) = start_writing(&test_box, &repo, "CARD-B", &["--after", &a]);
    let (code, submitted) = result_json(test_box.bay(&b_view).args(["merge"]));
    assert!(code == 0 || code == 2, "{submitted}");

    // WHEN B's status is read
    let read = test_box.bay_ok(&repo, &["status", &b]);

    // THEN it names A and where A stands
    let after = &read["detail"]["changes"][0]["operation"]["after"];
    assert_eq!(after[0]["change"], a.as_str(), "{read}");
    assert!(after[0]["operation"].is_null(), "{read}");
    let say = read["say"].as_str().unwrap();
    assert!(say.contains("which is not submitted yet"), "{say}");
    // AND the line's longest wait does not count B, which waits for A, not for the line
    let line = test_box.bay_ok(&repo, &["queue"]);
    let held = line["detail"]["arriving"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|row| row["change"] == b.as_str()));
    assert!(held, "B is not among the arrivals: {line}");
    assert!(line["detail"]["oldest_wait_s"].is_null(), "{line}");
}

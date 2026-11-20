//! G810 (a measurement, 2026-11-20: `bay hook tool.before` took a median 148 ms on one box and
//! 230 to 300 ms on a loaded Mac, nearly all of it 5 to 7 Git processes that opened the
//! repository; in Code Mode a pre-tool hook that does not finish refuses the call). A tool call
//! is judged with no Git process of the hook's own, once the repository was opened: the hook
//! reads what the last open read, while every file it rests on is as it was. A change to one (a
//! fetch that moves the trunk, a config file written) is read at the next call.
//!
//! Guards to mutate, in `src/repo.rs`: `Repo::open_quick` opening the repository every time (the
//! warm call runs Git); `Repo::from_opened` not checking its files (the next call after main's
//! config named a target judges by the old config, and a push to the target runs).

#![cfg(unix)]

use crate::support;

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;
use support::TestBox;

const CONFIG: &str = "main = \"main\"\n\n[agent]\nproduct = \"claude\"\n";

/// A repository whose agent product is Claude Code, and a change's view in it.
fn view(test_box: &TestBox) -> (PathBuf, PathBuf) {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &support::check_files("true", &support::passing_result(1), 1, 60),
    );
    test_box.commit_config(&repo, CONFIG);
    test_box.fake_claude(Some("2.1.302"));
    let (_, view) = test_box.start_and_edit(&repo);
    (repo, view)
}

/// `bay hook tool.before` for a Bash call of `command` in `view`, its Git calls traced to
/// `trace`: its exit code and process id.
fn judged(test_box: &TestBox, view: &Path, command: &str, trace: &Path) -> (Option<i32>, u32) {
    let payload = serde_json::json!({
        "session_id": "g810-session",
        "transcript_path": "/dev/null",
        "cwd": view,
        "permission_mode": "bypassPermissions",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "tool_use_id": "toolu_g810",
    });
    let mut child = test_box
        .bay_process(view)
        .args(["hook", "tool.before"])
        .env("BAY_TEST_GIT_TRACE", trace)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let pid = child.id();
    (child.wait().unwrap().code(), pid)
}

/// The Git calls process `pid` traced to `trace`.
fn git_calls(trace: &Path, pid: u32) -> Vec<String> {
    std::fs::read_to_string(trace)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["pid"] == pid)
        .map(|line| format!("{} {}", line["sub"], line["args"]))
        .collect()
}

#[test]
fn a_warm_tool_call_runs_no_git_process_of_its_own() {
    let test_box = TestBox::new();
    let (_repo, view) = view(&test_box);
    let trace = test_box.path().join("trace");
    // GIVEN a call that opened the repository
    let (code, _) = judged(&test_box, &view, "ls -la", &trace);
    assert_eq!(code, Some(0));
    // WHEN the session makes its next call
    let (code, pid) = judged(&test_box, &view, "ls -la", &trace);
    // THEN it is allowed, and the hook ran no Git process of its own
    assert_eq!(code, Some(0));
    let calls = git_calls(&trace, pid);
    assert!(calls.is_empty(), "the warm call ran Git: {calls:#?}");
}

#[test]
fn a_target_main_names_after_the_last_open_is_judged_at_the_next_call() {
    let test_box = TestBox::new();
    let (repo, view) = view(&test_box);
    let trace = test_box.path().join("trace");
    // GIVEN a call that opened the repository while main's config named no target: a push of
    // `staging` runs
    let (code, _) = judged(&test_box, &view, "git push origin staging", &trace);
    assert_eq!(
        code,
        Some(0),
        "a push of a branch no config names was denied"
    );
    // WHEN main's config names `staging` a target, and this clone fetched it
    test_box.commit_config(
        &repo,
        &format!("{CONFIG}\n[[target]]\nname = \"staging\"\n"),
    );
    // THEN the next call judges by it: the push is denied
    let (code, _) = judged(&test_box, &view, "git push origin staging", &trace);
    assert_eq!(code, Some(2), "a push of a target ran on the old config");
}

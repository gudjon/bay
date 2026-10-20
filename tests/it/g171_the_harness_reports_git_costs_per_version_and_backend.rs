//! G171. The cost harness (`examples/git_cost.rs`) reports Git's cost per bay command from
//! the trace a test build writes when `BAY_TEST_GIT_TRACE` names a file: one JSON line per
//! `git` the process spawns, with its subcommand, its microseconds and its exit code. The
//! numbers are only as good as the trace is complete, so the trace is held against a `git`
//! wrapper on `PATH` that logs every call: the same calls, by subcommand, and as many of them.
//! With the variable unset or empty, nothing is written.

// Each case runs shell scripts (a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

/// The subcommand of one call's arguments, by the rule the trace states: past `-C <dir>`,
/// `-c <key=value>` and other `-` options; `--version` is its own.
fn subcommand(args: &[&str]) -> String {
    let mut args = args.iter().copied();
    while let Some(arg) = args.next() {
        match arg {
            "-C" | "-c" => {
                args.next();
            }
            "--version" => return arg.to_owned(),
            _ if arg.starts_with('-') => {}
            _ => return arg.to_owned(),
        }
    }
    String::new()
}

/// The subcommands of the calls the wrapper logged, sorted.
fn logged(log: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let mut subs: Vec<String> = text
        .split('\x1e')
        .filter(|call| !call.is_empty())
        .map(|call| {
            let args: Vec<&str> = call
                .strip_suffix('\x1f')
                .unwrap_or(call)
                .split('\x1f')
                .collect();
            subcommand(&args)
        })
        .collect();
    subs.sort();
    subs
}

/// A repository with an unchecked trunk, and the wrapper logging every git call from here on.
fn seeded(test_box: &TestBox, log: &Path) -> std::path::PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    test_box.log_git_calls(log);
    repo
}

#[test]
fn a_status_traces_each_git_spawn_once() {
    let test_box = TestBox::new();
    let log = test_box.path().join("git-calls.log");
    let trace = test_box.path().join("git-trace.jsonl");
    let repo = seeded(&test_box, &log);

    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_TEST_GIT_TRACE", &trace)
            .arg("status"),
    );
    assert_eq!(code, 0, "{result}");

    let called = logged(&log);
    assert!(!called.is_empty(), "bay status ran no git at all");
    let text = std::fs::read_to_string(&trace).expect("the trace was written");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect();
    for line in &lines {
        assert!(line["us"].is_u64(), "{line}");
        assert!(line["exit"].is_i64() || line["exit"].is_null(), "{line}");
        assert!(line["pid"].is_u64(), "{line}");
    }
    let mut traced: Vec<String> = lines
        .iter()
        .map(|line| line["sub"].as_str().unwrap().to_owned())
        .collect();
    traced.sort();
    assert_eq!(
        traced, called,
        "one trace line per git spawn, by subcommand:\ntrace: {text}"
    );
}

#[test]
fn an_unset_or_empty_variable_traces_nothing() {
    let test_box = TestBox::new();
    let log = test_box.path().join("git-calls.log");
    let repo = seeded(&test_box, &log);
    let before: Vec<_> = std::fs::read_dir(test_box.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();

    let (code, result) = result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 0, "{result}");
    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_TEST_GIT_TRACE", "")
            .arg("status"),
    );
    assert_eq!(code, 0, "{result}");

    assert!(!logged(&log).is_empty(), "bay status ran no git at all");
    // Neither run left a file of its own in the box.
    let mut after: Vec<_> = std::fs::read_dir(test_box.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    after.retain(|name| !before.contains(name) && name.as_os_str() != "git-calls.log");
    assert!(after.is_empty(), "{after:?}");
}

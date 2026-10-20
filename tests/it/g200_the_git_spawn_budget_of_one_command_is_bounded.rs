//! G200. The git spawn budget of a bay command is published and held: `bay status` and
//! `bay start` on a small repository spawn at most a stated number of `git` processes, read
//! from the trace a test build writes when `BAY_TEST_GIT_TRACE` names a file (G171). Each
//! command lists its repository's config once per handle it opens, never one `git config`
//! per key, so its `config` spawns stay within a handful however many keys it reads.

use crate::support;

use assert_cmd::Command;
use std::collections::BTreeMap;

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

/// The most `git` spawns of `bay status` on the seeded repository, with no change open.
const STATUS_SPAWNS: usize = 80;
/// The most `git` spawns of `bay start` there.
const START_SPAWNS: usize = 45;
/// The most `git config` spawns of either command: one listing per repository handle it opens
/// (the opening check, the command, the coordination cache it writes at the end).
const CONFIG_SPAWNS: usize = 3;

/// The subcommands one run of `cmd` spawned, by count, from bay's own process only.
fn spawns(test_box: &TestBox, name: &str, cmd: &mut Command) -> (BTreeMap<String, usize>, Value) {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let (code, result) = result_json(cmd.env("BAY_TEST_GIT_TRACE", &trace));
    assert_eq!(code, 0, "{result}");
    let text = std::fs::read_to_string(&trace).expect("the trace was written");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let mut counts = BTreeMap::new();
    for line in lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref())
    {
        *counts
            .entry(line["sub"].as_str().unwrap_or_default().to_owned())
            .or_insert(0) += 1;
    }
    (counts, result)
}

fn assert_budget(command: &str, counts: &BTreeMap<String, usize>, most: usize) {
    let total: usize = counts.values().sum();
    let config = counts.get("config").copied().unwrap_or(0);
    assert!(
        total <= most,
        "bay {command} spawned {total} git processes, budget {most}: {counts:?}"
    );
    assert!(
        config <= CONFIG_SPAWNS,
        "bay {command} spawned {config} git config processes, budget {CONFIG_SPAWNS}: {counts:?}"
    );
}

fn seeded(test_box: &TestBox) -> std::path::PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    repo
}

#[test]
fn a_status_stays_within_its_git_spawn_budget() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let (counts, _) = spawns(&test_box, "status", test_box.bay(&repo).arg("status"));
    assert_budget("status", &counts, STATUS_SPAWNS);
}

#[test]
fn a_start_stays_within_its_git_spawn_budget() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let (counts, result) = spawns(
        &test_box,
        "start",
        test_box
            .bay(&repo)
            .args(["start", "CARD-1", "--paths", "src/"]),
    );
    assert!(result["detail"]["view"].is_string(), "{result}");
    assert_budget("start", &counts, START_SPAWNS);
}

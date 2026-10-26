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

/// W5 (a 139 s `bay status` on a queue of 87 changes): status reads the executor logs once,
/// in one batch, however many live changes it lists, never every log once per change, and every
/// chain in one batch, never one read per change or per state. With two changes open here and
/// the merges made in another clone, its `cat-file` spawns are the same after four as after two.
#[test]
fn a_status_reads_the_executor_logs_once_however_many_changes_it_lists() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    for n in 1..=2 {
        test_box.bay_ok(
            &repo,
            &[
                "start",
                &format!("OPEN-{n}"),
                "--paths",
                &format!("open{n}/"),
            ],
        );
    }
    // The merges come from another clone, so this one's views stay the same while the logs grow.
    let other = test_box.clone_of(&test_box.path().join("repo-remote.git"), "other");
    let merge = |n: usize| {
        let started = test_box.bay_ok(&other, &["start", &format!("CARD-{n}"), "--paths", "src/"]);
        let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(format!("src/n{n}.rs")), "fn n() {}\n").unwrap();
        let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{merged}");
    };
    let cat_files = |name: &str| {
        let (counts, listed) = spawns(&test_box, name, test_box.bay(&repo).arg("status"));
        assert!(listed.to_string().contains("OPEN-2"), "{listed}");
        counts.get("cat-file").copied().unwrap_or(0)
    };
    merge(1);
    merge(2);
    let two = cat_files("two");
    merge(3);
    merge(4);
    let four = cat_files("four");
    assert!(
        four <= two,
        "bay status read more objects one by one as the logs grew: {two} cat-file spawns after two merges, {four} after four"
    );
}

/// W5: a command pushes only the local chains the remote lacks or is behind on, never a stale
/// copy of a chain the remote has moved on: that push could only be refused (14 such pushes in
/// one 139 s `bay status`).
#[test]
fn a_status_never_pushes_a_chain_the_remote_moved_past() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/n.rs"), "fn n() {}\n").unwrap();
    let remote = test_box.path().join("repo-remote.git");
    let other = test_box.clone_of(&remote, "other");
    test_box.git_ok(
        &other,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/changes/*:refs/bay/changes/*",
        ],
    );
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    let (counts, _) = spawns(&test_box, "stale", test_box.bay(&other).arg("status"));
    assert_eq!(
        counts.get("push").copied().unwrap_or(0),
        0,
        "bay status pushed a stale chain: {counts:?}"
    );
}

/// A fleet finding (2026-10-26: 43 views, a named status took 5 minutes): `bay status <change>`
/// reads that change's own view and newest draft, never every view's chain state by state, so
/// its git spawns are the same with two other views open as with six.
#[test]
fn a_named_status_costs_the_same_however_many_other_views_are_open() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let open = |n: usize| {
        test_box.bay_ok(
            &repo,
            &["start", &format!("VIEW-{n}"), "--paths", &format!("v{n}/")],
        )["change"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let named = open(0);
    let (one, two) = (open(1), open(2));
    let total = |name: &str| {
        let (counts, _) = spawns(
            &test_box,
            name,
            test_box.bay(&repo).args(["status", &named]),
        );
        counts.values().sum::<usize>()
    };
    let with_two = total("two");
    for n in 3..=6 {
        open(n);
    }
    let with_six = total("six");
    assert!(
        with_six <= with_two,
        "bay status {named} spawned more git processes as other views opened ({one}, {two}, …): {with_two} with two, {with_six} with six"
    );
}

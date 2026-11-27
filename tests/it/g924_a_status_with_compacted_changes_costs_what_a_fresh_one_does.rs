//! G924 (plan S4: `bay status` on a store with 50,000 compacted changes within 10 % of a fresh
//! store). A listing reads live refs only, and a status of an old change reads one shard, so a
//! store's status costs what a fresh store's does, however many changes moved into the shard
//! indexes before it.
//!
//! Each time is the least of several runs, the two stores in turn, each run with its own sweep
//! (`BAY_TEST_SWEPT_S=0`). The case of 50,000 changes writes their records into the host's shards
//! directly, as old compactions left them, and is ignored by default: run it with
//! `cargo test --test it g924 -- --ignored`.
//!
//! Guard to mutate: in `src/status.rs`, `Plan::of` asking for the shard indexes in the sweep
//! (`crate::compact::refspec()`): the case of 50,000 changes fails, each listing asking for every
//! shard.

#![cfg(unix)]

use crate::support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use support::shards::{TwoRunners, index, pass, path, remaining, shard, two_runners, write_shards};
use support::{TestBox, check_files, passing_result, result_json};

/// Runs of each store's status; the least time counts.
const RUNS: usize = 11;

/// A store of a host with no change ever made, configured as [`two_runners`]'s.
fn fresh_store(test_box: &TestBox) -> PathBuf {
    let host = test_box.path().join("fresh.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("fresh-repo");
    test_box.add_origin(&repo, &host);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\nref_budget = 1\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.clone_of(&host, "fresh-store")
}

/// Merged and abandoned changes on [`two_runners`]'s host, moved into its shards by its first
/// runner until the host holds no ref of them.
fn compacted(test_box: &TestBox) -> TwoRunners {
    let runners = two_runners(test_box, 3, 1);
    for _ in 0..24 {
        if remaining(test_box, &runners.host, &runners.changes).is_empty() {
            break;
        }
        pass(test_box, &runners.runners[0]);
    }
    assert!(remaining(test_box, &runners.host, &runners.changes).is_empty());
    runners
}

/// A store of `host` holding a copy of each of its shard indexes, as a store that started a
/// change holds them.
fn old_store(test_box: &TestBox, host: &Path) -> PathBuf {
    let old = test_box.clone_of(host, "old-store");
    test_box.git_ok(
        &old,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/landed-index/*:refs/bay/landed-index/*",
        ],
    );
    old
}

/// How long one `bay status` in `dir` takes, with its own sweep.
fn status_time(test_box: &TestBox, dir: &Path) -> Duration {
    let began = Instant::now();
    let (code, status) = result_json(test_box.bay(dir).arg("status").env("BAY_TEST_SWEPT_S", "0"));
    let took = began.elapsed();
    assert_eq!(code, 0, "{status}");
    took
}

/// The least status time of `fresh` and of `old`, each warmed by one run first.
fn least_times(test_box: &TestBox, fresh: &Path, old: &Path) -> (Duration, Duration) {
    status_time(test_box, fresh);
    status_time(test_box, old);
    let (mut least_fresh, mut least_old) = (Duration::MAX, Duration::MAX);
    for _ in 0..RUNS {
        least_fresh = least_fresh.min(status_time(test_box, fresh));
        least_old = least_old.min(status_time(test_box, old));
    }
    (least_fresh, least_old)
}

/// Asserts `old`'s status time is within 10 % of `fresh`'s.
fn within_a_tenth(test_box: &TestBox, fresh: &Path, old: &Path, changes: usize) {
    let (fresh_time, old_time) = least_times(test_box, fresh, old);
    eprintln!(
        "G924: status of a fresh store {fresh_time:?}, of a store with {changes} compacted changes {old_time:?}"
    );
    assert!(
        old_time.as_secs_f64() <= fresh_time.as_secs_f64() * 1.10,
        "with {changes} compacted changes status took {old_time:?}, a fresh store's {fresh_time:?}"
    );
}

#[test]
fn a_status_with_compacted_changes_costs_what_a_fresh_one_does() {
    let test_box = TestBox::new();
    let fresh = fresh_store(&test_box);
    let runners = compacted(&test_box);
    let old = old_store(&test_box, &runners.host);
    within_a_tenth(&test_box, &fresh, &old, runners.changes.len());
}

/// The base-32 change id of `n`.
fn id_of(n: usize) -> String {
    let alphabet = b"abcdefghijklmnopqrstuvwxyz234567";
    (0..8)
        .map(|place| char::from(alphabet[(n >> (5 * place)) & 31]))
        .collect()
}

#[test]
#[ignore = "writes 50,000 compacted changes; run with --ignored"]
fn a_status_with_50000_compacted_changes_costs_what_a_fresh_one_does() {
    const CHANGES: usize = 50_000;
    let test_box = TestBox::new();
    let fresh = fresh_store(&test_box);
    let runners = compacted(&test_box);
    let host = &runners.host;
    // Each old change keeps the records of a compacted change of this host, under its own names.
    let like = &runners.changes[0];
    let entries_of = |prefix: &str| -> Vec<(String, String)> {
        let name = like.iter().find(|name| name.starts_with(prefix)).unwrap();
        let at = format!("{}:{}", index(name), path(name));
        if test_box.git_bare(host, &["cat-file", "-t", &at]) == "blob" {
            return vec![(String::new(), test_box.git_bare(host, &["rev-parse", &at]))];
        }
        test_box
            .git_bare(host, &["ls-tree", &at])
            .lines()
            .map(|line| {
                let (meta, file) = line.split_once('\t').unwrap();
                (
                    format!("/{file}"),
                    meta.split(' ').nth(2).unwrap().to_owned(),
                )
            })
            .collect()
    };
    let kept: Vec<(&str, Vec<(String, String)>)> = [
        "refs/bay/changes/",
        "refs/bay/requests/",
        "refs/bay/landed/",
    ]
    .into_iter()
    .map(|prefix| (prefix, entries_of(prefix)))
    .collect();
    let mut shards: BTreeMap<u8, Vec<(String, String)>> = BTreeMap::new();
    for n in 0..CHANGES {
        let id = id_of(n + (1 << 30));
        for (prefix, files) in &kept {
            let name = format!("{prefix}{id}");
            let list = shards.entry(shard(&name)).or_default();
            for (file, blob) in files {
                list.push((format!("{}{file}", path(&name)), blob.clone()));
            }
        }
    }
    write_shards(&test_box, host, &shards);
    let old = old_store(&test_box, host);
    within_a_tenth(&test_box, &fresh, &old, CHANGES + runners.changes.len());
    // AND a status of an old change still answers, from its one shard
    let id = like[0].trim_start_matches("refs/bay/changes/");
    let status = test_box.bay_ok(&old, &["status", id]);
    assert_eq!(
        status["detail"]["changes"][0]["state"], "merged",
        "{status}"
    );
}

//! G802 (a fleet measurement, 2026-11-20: in 8.6 days one line's two runners bisected 28 red
//! batches one prefix after another, the line's head held 10.3 h in all; five of them found a
//! passing prefix of four to seven members and landed none of it, lost to the head's deadline or
//! to a member that left before the push). A red batch lands its first passing prefix at once.
//!
//! Once the whole batch failed, the search halves it until a prefix passes; that prefix lands at
//! once (as a batch of two members or more), and the members past it go back to the line, where
//! the next head's batch reads what the search recorded on their trees: no tree is checked twice.
//! Each prefix's records reach the remote as it finishes, so a runner that stops mid-search keeps
//! what it checked.
//!
//! Guards to mutate: in `src/throughput.rs`, `Bisect::next_prefix`'s `self.passed == 0` (the
//! search goes on past a passing prefix to the largest, and the first landing holds five members,
//! not three); in `src/publish.rs`, `bisect_batch`'s `failed_before` (the batch of the members
//! that came back checks the whole batch's tree again) and its push of each prefix's records
//! (pushed only once the search ends, the whole batch's record is lost with a runner stopped
//! mid-search).

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};

use support::PRODUCER;
use support::queued::{Check, Held, host, ran_for};

/// A host whose check fails a candidate that holds `src/bad.rs`, or whenever `fails` holds,
/// batching up to eight at any failure rate, and six changes in line, the last holding
/// `src/bad.rs`. Returns the host and the changes in queue order.
fn line(fails: &str) -> (Held, Vec<String>) {
    let fails = format!("[ -e src/bad.rs ] || {{ {fails}; }}");
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\nbatch = { min = 2, max = 8, max_failure_rate = 1.0 }\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: &fails,
    });
    let ids = ["one", "two", "three", "four", "five", "bad"]
        .iter()
        .map(|file| held.queue(file, "src"))
        .collect();
    (held, ids)
}

/// Each change id on the host's main with its `Bay-Batch` trailer (empty for none), oldest
/// first.
fn landings(held: &Held) -> Vec<(String, String)> {
    held.test_box
        .git_bare(
            &held.remote,
            &[
                "log",
                "--reverse",
                "--format=%(trailers:key=Change-Id,valueonly,separator=)|%(trailers:key=Bay-Batch,valueonly,separator=)",
                "main",
            ],
        )
        .lines()
        .filter_map(|line| line.split_once('|'))
        .filter(|(id, _)| !id.trim().is_empty())
        .map(|(id, batch)| (id.trim().to_owned(), batch.trim().to_owned()))
        .collect()
}

#[test]
fn a_red_batch_lands_its_first_passing_prefix_at_once() {
    // GIVEN six changes in line, the sixth failing the check
    let (held, ids) = line("false");
    let executor = held.executor("exec", PRODUCER);
    // WHEN a runner's pass checks them as one batch
    let pass = held.pass(&executor);

    // THEN the whole batch fails, its first half passes, and those three land at once, as one
    // batch of three
    let landed = landings(&held);
    let first: Vec<&(String, String)> = landed
        .iter()
        .filter(|(id, _)| ids[..3].contains(id))
        .collect();
    assert_eq!(first.len(), 3, "{landed:?}");
    let batch = first[0].1.split_whitespace().next().unwrap_or_default();
    assert_eq!(
        first
            .iter()
            .map(|(_, tag)| tag.as_str())
            .collect::<Vec<_>>(),
        vec![
            format!("{batch} 1/3"),
            format!("{batch} 2/3"),
            format!("{batch} 3/3")
        ],
        "the first three did not land at once as a batch of three: {landed:?}"
    );
    // AND the rest went back to the line: the two that pass land after them, and the sixth is
    // refused
    let order: Vec<&String> = landed.iter().map(|(id, _)| id).collect();
    assert_eq!(order, ids[..5].iter().collect::<Vec<_>>(), "{landed:?}");
    assert_eq!(ran_for(&pass, &ids[5])["reason"], "CHECK_FAILED", "{pass}");
    // AND the batches after the first read what the search recorded: the whole batch's tree is
    // checked again only as the sixth change's own candidate, which refuses it on its own base
    let requests = held.requests();
    let whole = &requests[0]["candidate"]["tree"];
    let again: Vec<&serde_json::Value> = requests[1..]
        .iter()
        .filter(|request| &request["candidate"]["tree"] == whole)
        .collect();
    assert_eq!(
        again.len(),
        1,
        "a batch checked the whole batch's tree again: {requests:?}"
    );
    assert_eq!(
        again[0]["changed_paths"],
        serde_json::json!(["src/bad.rs"]),
        "{requests:?}"
    );
}

#[test]
fn a_runner_stopped_mid_search_keeps_the_prefixes_it_checked() {
    // GIVEN six changes in line, the sixth failing, and a check whose second run takes a while
    let (held, _ids) = line("[ \"$n\" = 2 ] && sleep 30 && false");
    let executor = held.executor("exec", PRODUCER);
    let mut runner = held
        .test_box
        .bay_process(&executor)
        .args(["runner", "run", "--once"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // WHEN the runner stops while it checks the batch's second prefix
    let began = Instant::now();
    while held.requests().len() < 2 {
        assert!(
            began.elapsed() < Duration::from_secs(120),
            "the search never reached its second prefix"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = runner.kill();
    let _ = runner.wait();

    // THEN the host holds the record of the whole batch, checked first
    let requests = held.requests();
    let whole = requests[0]["candidate"]["tree"].as_str().unwrap();
    assert!(
        noted(&held, whole),
        "the whole batch's record was lost with the runner: {requests:?}"
    );
}

/// Whether the host holds a record of [`PRODUCER`]'s on `tree`.
fn noted(held: &Held, tree: &str) -> bool {
    let notes = format!("refs/notes/bay/evidence/{PRODUCER}");
    let listed = held.test_box.git_bare(
        &held.remote,
        &["for-each-ref", "--format=%(refname)", &notes],
    );
    listed.trim() == notes
        && held
            .test_box
            .git_bare(&held.remote, &["ls-tree", "-r", "--name-only", &notes])
            .lines()
            .any(|path| path.replace('/', "") == tree)
}

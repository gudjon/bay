//! G789 (a fleet measurement, 2026-11-18, at load 70 to 100: after G788 a pass woken by a
//! publish spent about 25 s on its setup, its fetches, sweeps, presence and reads of every chain,
//! before it visited the new head; and the runner that landed spent p50 24 s, p90 38 s, on what
//! followed its landing, its records, logs and conflict probes, before it took the next head).
//! The next head's visit starts within a few round trips of a landing, on a loaded box.
//!
//! - A runner woken between passes by a publish visits the new head before the rest of its pass:
//!   it claims it before it reads every chain, lists the remote's land requests, or probes the
//!   changes in line for conflicts.
//! - A runner that lands takes the next head before it probes the line for conflicts, and its
//!   landing's record and terminal record ride in its landing's own push (G864).
//! - BACKLOG 277 (a fleet measurement, 2026-11-24: an idle pass pushed its presence on its own
//!   path, 4.8 to 6.1 s of a pass of about 15 s): a pass pushes its presence on a thread of its
//!   own, and `--once` still reports the push.
//!
//! Read from the trace a test build writes when `BAY_TEST_GIT_TRACE` names a file (G171).
//!
//! Guards to mutate, in `src/executor.rs`: `pass`'s heads taken first (drop the loop, and the
//! woken runner reads every chain first); the conflict probe after the abandoned-head take (put
//! it back in `run_op`, and the lander probes first). In `src/publish.rs`, `land_with_records`'s
//! chain in `riding.specs` (push no chain, and the records go in a push after the landing);
//! G867, `Riding::lease` (return false, and the lease the check beat is released in a push of its
//! own before the next head's claim).

#![cfg(unix)]

use crate::support;

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use support::PRODUCER;
use support::queued::{Check, Held, Trust, host_under};

/// Two checks, one per environment: `src/` wakes the one in `env-a`, `lib/` the one in `env-b`.
const POLICY: &str = "revision = 1\n\n[policy]\nreuse_unsigned = true\n\n\
    [[check]]\nid = \"front\"\nenvironment = \"env-a\"\ncommand = \"scripts/bay-check\"\n\
    adapter = [\"scripts/bay-check\"]\nwhen_paths = [\"src/**\"]\nexpected_minimum = 1\ntimeout_s = 900\n\n\
    [[check]]\nid = \"back\"\nenvironment = \"env-b\"\ncommand = \"scripts/bay-check\"\n\
    adapter = [\"scripts/bay-check\"]\nwhen_paths = [\"lib/**\"]\nexpected_minimum = 1\ntimeout_s = 900\n";

/// The runner's wait between passes: far longer than any bound below.
const INTERVAL_S: &str = "900";

/// How soon after the publish the next head must be claimed: never a wait out of
/// [`INTERVAL_S`]. What stands before the claim is read from the trace.
const BOUND_S: u64 = 120;

/// An executor clone of the host named `name`, serving `environment` (every one with `None`).
fn runner(held: &Held, name: &str, environment: Option<&str>) -> PathBuf {
    let clone = held.executor(name, PRODUCER);
    let git = |args: &[&str]| held.test_box.git_ok(&clone, args);
    git(&["config", "bay.runnerName", name]);
    git(&["config", "bay.producerName", &format!("{name}-checks")]);
    if let Some(environment) = environment {
        git(&["config", "bay.checkEnvironment", environment]);
    }
    clone
}

/// A looping runner in `clone` tracing its Git calls to `trace`, its journal gathered.
fn looping(
    held: &Held,
    clone: &Path,
    trace: &Path,
) -> (std::process::Child, Arc<Mutex<Vec<String>>>) {
    let mut child = held
        .test_box
        .bay_process(clone)
        .env("BAY_TEST_GIT_TRACE", trace)
        .args(["--json", "runner", "run", "--interval-s", INTERVAL_S])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let journal = Arc::new(Mutex::new(Vec::new()));
    let gathered = Arc::clone(&journal);
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            gathered.lock().unwrap().push(line);
        }
    });
    (child, journal)
}

/// Waits up to `limit` for `done`.
fn until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while began.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}

/// The commit time of the first commit on change `id`'s chain at the host whose subject starts
/// with `subject`.
fn written(held: &Held, id: &str, subject: &str) -> Option<u64> {
    held.test_box
        .git_bare(
            &held.remote,
            &[
                "log",
                "--reverse",
                "--format=%ct %s",
                &format!("refs/bay/changes/{id}"),
            ],
        )
        .lines()
        .find_map(|line| {
            let (at, said) = line.split_once(' ')?;
            said.starts_with(subject).then(|| at.parse().ok()).flatten()
        })
}

/// One Git call of a trace: its arguments joined, and whether the pass's own thread made it.
struct Call {
    args: String,
    pass: bool,
}

/// The calls in `trace` after the last that `from` matches before the first that `to` matches,
/// neither included, in start order: the pass's own thread's only with `pass`, else every
/// thread's too (the beat's, `crate::presence`).
fn between(
    trace: &Path,
    from: impl Fn(&str) -> bool,
    to: impl Fn(&str) -> bool,
    pass: bool,
) -> Vec<String> {
    let mut calls: Vec<(u64, Call)> = std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| {
            let call: Value = serde_json::from_str(line).unwrap();
            let args: Vec<&str> = call["args"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let start = call["end"].as_u64().unwrap() - call["us"].as_u64().unwrap();
            let pass = call["thread"].as_str() == Some("ThreadId(1)");
            (
                start,
                Call {
                    args: args.join(" "),
                    pass,
                },
            )
        })
        .collect();
    calls.sort_by_key(|(start, _)| *start);
    let calls: Vec<Call> = calls
        .into_iter()
        .map(|(_, call)| call)
        .filter(|call| call.pass || !pass)
        .collect();
    let end = calls
        .iter()
        .position(|call| to(&call.args))
        .expect("the trace holds the call that ends the window");
    let start = calls[..end]
        .iter()
        .rposition(|call| from(&call.args))
        .expect("the trace holds the call that begins the window");
    calls[start + 1..end]
        .iter()
        .map(|call| call.args.clone())
        .collect()
}

/// The setup a pass makes after its heads, which no head's visit waits for: every chain
/// fetched, the remote's land requests listed, and the line probed for conflicts.
fn setup(args: &str) -> bool {
    args.contains("refs/bay/changes/*:refs/bay/changes/*")
        || (args.contains("ls-remote") && args.contains("refs/bay/requests/*"))
        || args.contains("merge-tree --write-tree --quiet")
}

#[test]
fn a_runner_woken_by_a_publish_visits_the_new_head_before_the_rest_of_its_pass() {
    let held = host_under(
        &Check {
            config: "[runner]\nlease_s = 60\n",
            policy: Some(POLICY),
            seed: &[("src/a.rs", "fn a() {}\n"), ("lib/b.rs", "fn b() {}\n")],
            hold_first: true,
            fails: "false",
        },
        Trust::Unsigned,
    );
    let first = held.queue("first", "src");
    let second = held.queue("second", "lib");
    // Changes waiting in line behind it, which the rest of a pass reads and probes.
    let _waiting: Vec<String> = ["third", "fourth"]
        .iter()
        .map(|name| held.queue(name, "lib"))
        .collect();
    let publisher = runner(&held, "exec-a", Some("env-a"));
    let holder = held.spawn(&publisher);
    held.wait_held();

    // GIVEN the runner that serves the next change made its pass and waits a quarter of an hour
    let woken = runner(&held, "exec-b", Some("env-b"));
    let trace = held.test_box.path().join("woken.trace");
    let (mut child, journal) = looping(&held, &woken, &trace);
    let visited =
        until(Duration::from_secs(180), || {
            journal.lock().unwrap().iter().any(|line| {
                line.contains(&format!("{first}.1")) && line.contains("CHECK_UNAVAILABLE")
            })
        });
    assert!(visited, "{:?}", journal.lock().unwrap());
    std::thread::sleep(Duration::from_secs(2));

    // WHEN another runner publishes the head, and cannot run the change behind it
    held.go();
    let out = holder.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let landed = written(&held, &first, &format!("landed {first}.1")).unwrap();
    let claim = format!("claim {second}.1");
    let claimed = until(Duration::from_secs(BOUND_S + 20), || {
        written(&held, &second, &claim).is_some()
    });
    let _ = child.kill();
    let _ = child.wait();
    let at = written(&held, &second, &claim);
    assert!(
        claimed && at.is_some_and(|at| at.saturating_sub(landed) <= BOUND_S),
        "the next head was claimed {:?} s after the publish: {:?}",
        at.map(|at| at.saturating_sub(landed)),
        journal.lock().unwrap()
    );

    // THEN from the wait's last look at the line to its claim, the woken pass read the head and
    // what its visit needs, and nothing of the rest of its setup
    let before = between(
        &trace,
        |args| args.contains("ls-remote") && args.contains("refs/bay/queue/*"),
        |args| args.contains("commit-tree") && args.contains(&claim),
        true,
    );
    let setup: Vec<&String> = before.iter().filter(|args| setup(args)).collect();
    assert!(
        setup.is_empty(),
        "the woken pass made its setup before it claimed the new head: {setup:#?}\nall: {before:#?}"
    );
}

#[test]
fn a_runner_that_lands_takes_the_next_head_before_its_probes_and_records_in_one_push() {
    let held = host_under(
        &Check {
            config: "[runner]\nlease_s = 60\n",
            policy: Some(POLICY),
            seed: &[("src/a.rs", "fn a() {}\n")],
            hold_first: true,
            fails: "false",
        },
        Trust::Unsigned,
    );
    let first = held.queue("first", "src");
    let second = held.queue("second", "src");
    // Changes waiting in line behind the next head, which a landing's probe tests.
    let _waiting: Vec<String> = ["third", "fourth"]
        .iter()
        .map(|name| held.queue(name, "src"))
        .collect();

    // GIVEN one runner that read the line's chains before (an earlier pass), its check of the
    // head held
    let clone = runner(&held, "exec-a", Some("env-a"));
    held.test_box.git_ok(
        &clone,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/changes/*:refs/bay/changes/*",
        ],
    );
    let trace = held.test_box.path().join("lander.trace");
    let (mut child, journal) = looping(&held, &clone, &trace);
    held.wait_held();

    // WHEN the check passes and the runner lands the head, and the next
    held.go();
    let claim = format!("claim {second}.1");
    let next = format!("landed {second}.1");
    // The trace holds a call once it returns, and since G864 the remote holds a landing's record
    // while its push has yet to return: so wait for both.
    let lands_second = format!("landed/{second}");
    let drained = until(Duration::from_secs(BOUND_S + 20), || {
        written(&held, &second, &next).is_some()
            && std::fs::read_to_string(&trace).is_ok_and(|calls| {
                calls
                    .lines()
                    .any(|call| call.contains("\"push\"") && call.contains(&lands_second))
            })
    });
    let _ = child.kill();
    let _ = child.wait();
    let landed = written(&held, &first, &format!("landed {first}.1"));
    let at = written(&held, &second, &claim);
    assert!(
        drained && landed.is_some_and(|landed| at.is_some_and(|at| at - landed <= BOUND_S)),
        "the next head was not claimed soon after the landing: {:?}",
        journal.lock().unwrap()
    );

    // THEN between its landing's push and its claim of the next head, the runner probed nothing
    // and pushed nothing: the landing's record and its terminal record rode in that push
    let main = "refs/heads/main";
    let landing = |args: &str| args.contains("push") && args.contains(main);
    let claiming = |args: &str| args.contains("commit-tree") && args.contains(&claim);
    let probe = |args: &str| args.contains("merge-tree --write-tree --quiet");
    let before = between(&trace, landing, claiming, true);
    let probes: Vec<&String> = before.iter().filter(|args| probe(args)).collect();
    assert!(
        probes.is_empty(),
        "the runner probed the line before it took the next head: {probes:#?}\nall: {before:#?}"
    );
    // The line behind hears of its conflicts with the new main all the same: beside the next
    // head's visit, on the beat's own thread.
    let lands_next = |args: &str| landing(args) && args.contains(&format!("landed/{second}"));
    let beside = between(&trace, landing, lands_next, false);
    assert!(
        beside.iter().any(|args| probe(args)),
        "the line was never probed after the landing: {beside:#?}"
    );
    let pushes: Vec<&String> = before
        .iter()
        .filter(|args| args.split(' ').any(|word| word == "push"))
        .collect();
    let lands_first = std::fs::read_to_string(&trace)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let call: Value = serde_json::from_str(line).ok()?;
            let args: Vec<&str> = call["args"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            Some(args.join(" "))
        })
        .find(|args| landing(args) && args.contains(&format!("landed/{first}")))
        .expect("the trace holds the head's landing");
    assert!(
        pushes.is_empty()
            && lands_first.contains(&format!("refs/bay/changes/{first}"))
            && lands_first.contains("refs/bay/log/exec-a"),
        "the landing's records took {} pushes of their own before the next head: {pushes:#?}\nlanding: {lands_first}",
        pushes.len()
    );
    // G867: and no lease of the landed head stays on the remote: its release rode that push
    let lease = bay::lease::lease_ref(&first);
    let left = held.test_box.git_bare(
        &held.remote,
        &["for-each-ref", "--format=%(refname)", &lease],
    );
    assert!(
        left.trim().is_empty(),
        "the landed head's lease stays: {left}"
    );
}

/// BACKLOG 277: a pass pushes its presence beside it, never on its own thread, and `--once`
/// waits for the push before it reports it.
#[test]
fn a_pass_pushes_its_presence_beside_it_and_once_reports_the_push() {
    let test_box = support::TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &support::unchecked_files(),
    );
    let executor = test_box.clone_of(&remote, "executor");
    let trace = test_box.path().join("trace.jsonl");
    let (code, once) = support::result_json(
        test_box
            .bay(&executor)
            .env("BAY_TEST_GIT_TRACE", &trace)
            .args(["runner", "run", "--once"]),
    );
    assert_eq!(code, 0, "{once}");
    assert_eq!(once["detail"]["presence"]["pushed"], true, "{once}");
    let pushes: Vec<Value> = std::fs::read_to_string(&trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|call| {
            let args = call["args"].to_string();
            args.contains("\"push\"") && args.contains("refs/bay/executor/")
        })
        .collect();
    assert_eq!(pushes.len(), 1, "one presence push: {pushes:?}");
    assert_ne!(pushes[0]["thread"], "ThreadId(1)", "{pushes:?}");
}

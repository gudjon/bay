//! G801 (a fleet measurement, 2026-11-19: in 3 h one box's two runners made 122 visits to 25
//! heads whose checks only another live runner's box serves, a median 5 s each, mostly a fetch
//! of every presence; a head was visited again a median 61 s after the last). A looping runner
//! that left the queue's head to another live runner, whose box serves its checks, does not
//! visit it again, nor fetch any presence for it, while that runner's presence, as each pass's
//! fetch of the chains brings it, stays live, the head's chain stays as it was, the line's head
//! did not move, and main's rules did not change: its policy and its red post checks, which name
//! the checks the head needs and so the boxes that serve it. A move of main that changes neither
//! leaves the head left. Once no presence that serves it is live, or main's policy changed, it
//! visits the head again.
//!
//! Guards to mutate, in `src/executor.rs`: `left_until` answering `None` (each pass visits the
//! head again, and fetches every presence); `Parked::holds` and `keep_stands` ignoring a stand's
//! time (the head is never visited again after the runner it was left to stops); `keep_stands`
//! not renewing a stand (the head is visited again when the presence it was first left under
//! may read stale, though that runner beats); `Main::at` reading main's commit for a leave too
//! (the head is visited again each time main moves); `Main::at` reading no policy (the head is
//! not visited again once main's policy changed).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, check_files, result_json};

/// A runner's clone of `remote` named `name`, which declares `environment`.
fn runner(test_box: &TestBox, remote: &Path, name: &str, environment: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    test_box.git_ok(&clone, &["config", "bay.checkEnvironment", environment]);
    clone
}

/// A line whose head's one check runs in env-b, a live runner whose box serves env-b (its
/// presence names a cadence of five minutes, so it reads as live for 930 s), and the clone of a
/// runner whose box serves only env-a. Returns the box, the remote, the head's operation and
/// that clone.
fn line() -> (TestBox, PathBuf, String, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files: Vec<(String, String)> =
        check_files(&test_box.relay("true"), &support::passing_result(1), 1, 60)
            .into_iter()
            .map(|(path, text)| (path, text.replace("\"local\"", "\"env-b\"")))
            .chain([(
                ".bay/config.toml".to_owned(),
                "[runner]\nlease_s = 60\nheartbeat_s = 1\n".to_owned(),
            )])
            .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let serving = runner(&test_box, &remote, "exec-b", "env-b");
    let published = test_box.bay_ok(
        &serving,
        &["runner", "run", "--once", "--interval-s", "300"],
    );
    assert_eq!(
        published["detail"]["presence"]["pushed"], true,
        "{published}"
    );
    let started = test_box.bay_ok(&repo, &["start", "code", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/b.rs"), "fn b() {}\n").unwrap();
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    let op = queued["op_id"].as_str().unwrap().to_owned();
    let id = queued["change"].as_str().unwrap().to_owned();
    let handle = bay::repo::Repo::open(&repo).unwrap();
    bay::queue::join(&handle, None, &op, &id).unwrap();
    let other = runner(&test_box, &remote, "exec-a", "env-a");
    (test_box, remote, op, other)
}

/// A looping runner in `clone`, its reader's clock `skew_s` ahead: its journal and its Git
/// calls go to files under the box named `name`.
fn looping(test_box: &TestBox, clone: &Path, name: &str, skew_s: u64) -> std::process::Child {
    let dir = test_box.path();
    test_box
        .bay_process(clone)
        .args(["--json", "runner", "run", "--interval-s", "1"])
        .env("BAY_TEST_PRESENCE_CLOCK_SKEW_S", skew_s.to_string())
        .env("BAY_TEST_GIT_TRACE", dir.join(format!("{name}-trace")))
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(dir.join(format!("{name}-journal"))).unwrap())
        .spawn()
        .unwrap()
}

/// The runner `name`'s visits to `op` that left it waiting for a worker, and its passes (each
/// fetches every chain once).
fn seen(test_box: &TestBox, name: &str, op: &str) -> (usize, usize) {
    let journal = std::fs::read_to_string(test_box.path().join(format!("{name}-journal")))
        .unwrap_or_default();
    let visits = journal
        .lines()
        .filter(|line| line.contains(&format!("{op} [")) && line.contains("waiting for worker"))
        .count();
    (visits, fetches(test_box, name, "refs/bay/changes/*"))
}

/// The fetches of runner `name` whose arguments hold `what`, and not every chain unless that is
/// `what`: a visit's own fetch of the presences, apart from a pass's fetch of every chain, which
/// brings them too.
fn fetches(test_box: &TestBox, name: &str, what: &str) -> usize {
    let chains = "refs/bay/changes/*";
    std::fs::read_to_string(test_box.path().join(format!("{name}-trace")))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["sub"] == "fetch")
        .map(|line| line["args"].to_string())
        .filter(|args| args.contains(what) && (what == chains || !args.contains(chains)))
        .count()
}

/// The reader's clock skew under which the presence `exec-b` last pushed to `remote`, and any
/// it pushes later, reads stale `left_s` seconds after it was written.
fn skew_for(test_box: &TestBox, remote: &Path, left_s: u64) -> u64 {
    let presence = test_box.git_bare(remote, &["show", "refs/bay/executor/exec-b:presence.json"]);
    let presence: Value = serde_json::from_str(&presence).unwrap();
    let cadence = presence["cadence_s"].as_u64().unwrap();
    cadence * 3 + 30 - left_s
}

/// A clone of `remote` named `name`, made through git's transport: the looping runner pushes
/// into `remote` meanwhile, and a local clone, which copies the remote's object files, fails
/// when a push's incoming directory goes away while it copies.
fn mover(test_box: &TestBox, remote: &Path, name: &str) -> PathBuf {
    let dir = test_box.path().join(name);
    test_box.git_ok(
        test_box.path(),
        &[
            "clone",
            "-q",
            "--no-local",
            remote.to_str().unwrap(),
            dir.to_str().unwrap(),
        ],
    );
    dir
}

/// Moves main from a clone named `name`: `edit` changes its files, which one commit records.
/// Returns main's new commit.
fn move_main(test_box: &TestBox, remote: &Path, name: &str, edit: impl FnOnce(&Path)) -> String {
    let clone = mover(test_box, remote, name);
    edit(&clone);
    test_box.git_ok(&clone, &["add", "-A"]);
    test_box.git_ok(
        &clone,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            name,
        ],
    );
    test_box.git_ok(&clone, &["push", "-q", "origin", "HEAD:main"]);
    test_box
        .git_ok(&clone, &["rev-parse", "HEAD"])
        .trim()
        .to_owned()
}

/// Whether the runner's store in `clone` read main at `tip`.
fn read_main(test_box: &TestBox, clone: &Path, tip: &str) -> bool {
    test_box
        .git_ok(clone, &["rev-parse", "refs/remotes/origin/main"])
        .trim()
        == tip
}

/// Waits until `done` holds, at most `within`.
fn until(within: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let began = Instant::now();
    while !done() {
        assert!(began.elapsed() < within, "{what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn a_head_left_to_a_live_runner_is_not_visited_again_while_it_is_live() {
    let (test_box, _remote, op, other) = line();
    // GIVEN a looping runner whose box cannot serve the head
    let mut child = looping(&test_box, &other, "a", 0);
    // WHEN it passes four times beside the live runner that serves it
    until(
        Duration::from_secs(120),
        "the runner did not pass four times",
        || seen(&test_box, "a", &op).1 >= 4,
    );
    let _ = child.kill();
    let _ = child.wait();
    // THEN it visited the head once, and fetched the presences once for it, besides the one
    // fetch at its start that finds whether a live runner writes as its producer (G807)
    let (visits, passes) = seen(&test_box, "a", &op);
    let journal = std::fs::read_to_string(test_box.path().join("a-journal")).unwrap_or_default();
    assert_eq!(visits, 1, "{visits} visits in {passes} passes: {journal}");
    assert_eq!(
        fetches(&test_box, "a", "refs/bay/executor/*"),
        2,
        "the runner fetched the presences again: {journal}"
    );
}

#[test]
fn a_head_left_to_a_runner_that_may_read_stale_is_visited_again() {
    let (test_box, remote, op, other) = line();
    // GIVEN the live runner's presence reads stale to the looping runner within 20 s
    let presence = test_box.git_bare(&remote, &["show", "refs/bay/executor/exec-b:presence.json"]);
    let presence: Value = serde_json::from_str(&presence).unwrap();
    let at = bay::clock::unix_of(presence["at"].as_str().unwrap()).unwrap();
    let since = bay::live::unix_now().saturating_sub(at);
    let skew = skew_for(&test_box, &remote, 20).saturating_sub(since);
    let mut child = looping(&test_box, &other, "a", skew);
    until(
        Duration::from_secs(60),
        "the runner never left the head to the live runner",
        || seen(&test_box, "a", &op).0 >= 1,
    );
    // WHEN that presence may read stale, with no beat of it since
    // THEN the looping runner visits the head again
    until(
        Duration::from_secs(90),
        "the runner never visited the head again",
        || seen(&test_box, "a", &op).0 >= 2,
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn a_head_left_to_a_runner_that_beats_is_not_visited_again_past_its_first_window() {
    let (test_box, remote, op, other) = line();
    let serving = test_box.path().join("exec-b");
    // GIVEN each presence of the live runner reads stale to the looping runner 20 s after it
    // was written
    let presence = test_box.git_bare(&remote, &["show", "refs/bay/executor/exec-b:presence.json"]);
    let presence: Value = serde_json::from_str(&presence).unwrap();
    let at = bay::clock::unix_of(presence["at"].as_str().unwrap()).unwrap();
    let since = bay::live::unix_now().saturating_sub(at);
    let skew = skew_for(&test_box, &remote, 20);
    let mut child = looping(&test_box, &other, "a", skew.saturating_sub(since));
    until(
        Duration::from_secs(60),
        "the runner never left the head to the live runner",
        || seen(&test_box, "a", &op).0 >= 1,
    );
    // WHEN that runner beats every 3 s, for 40 s
    let handle = bay::repo::Repo::open(&serving).unwrap();
    let began = Instant::now();
    while began.elapsed() < Duration::from_secs(40) {
        let mut beat = bay::presence::Beat::new(None, Duration::from_secs(300));
        beat.publish(&handle, None, 1);
        std::thread::sleep(Duration::from_secs(3));
    }
    let _ = child.kill();
    let _ = child.wait();
    // THEN the looping runner visited the head once, past the window its first presence gave
    let (visits, passes) = seen(&test_box, "a", &op);
    let journal = std::fs::read_to_string(test_box.path().join("a-journal")).unwrap_or_default();
    assert_eq!(visits, 1, "{visits} visits in {passes} passes: {journal}");
}

#[test]
fn a_head_left_to_a_live_runner_is_not_visited_again_when_main_moves_under_the_same_rules() {
    let (test_box, remote, op, other) = line();
    // GIVEN a looping runner that left the head to the live runner
    let mut child = looping(&test_box, &other, "a", 0);
    until(
        Duration::from_secs(60),
        "the runner never left the head to the live runner",
        || seen(&test_box, "a", &op).0 >= 1,
    );
    // WHEN main moves twice, changing neither its policy nor its red post checks, and the
    // runner reads each move
    for n in 1..=2 {
        let tip = move_main(&test_box, &remote, &format!("mover-{n}"), |clone| {
            std::fs::write(clone.join(format!("moved-{n}.md")), "moved\n").unwrap();
        });
        until(
            Duration::from_secs(60),
            "the runner never read main's move",
            || read_main(&test_box, &other, &tip),
        );
        let passes = seen(&test_box, "a", &op).1;
        until(
            Duration::from_secs(60),
            "the runner did not pass twice after it read main's move",
            || seen(&test_box, "a", &op).1 >= passes + 2,
        );
    }
    let _ = child.kill();
    let _ = child.wait();
    // THEN it visited the head once
    let (visits, passes) = seen(&test_box, "a", &op);
    let journal = std::fs::read_to_string(test_box.path().join("a-journal")).unwrap_or_default();
    assert_eq!(visits, 1, "{visits} visits in {passes} passes: {journal}");
}

#[test]
fn a_head_left_to_a_live_runner_is_visited_again_once_mains_policy_changed() {
    let (test_box, remote, op, other) = line();
    // GIVEN a looping runner that left the head to the live runner
    let mut child = looping(&test_box, &other, "a", 0);
    until(
        Duration::from_secs(60),
        "the runner never left the head to the live runner",
        || seen(&test_box, "a", &op).0 >= 1,
    );
    // WHEN main's policy changes
    let tip = move_main(&test_box, &remote, "mover", |clone| {
        let policy = clone.join(".bay/policy.toml");
        let text = std::fs::read_to_string(&policy).unwrap();
        std::fs::write(&policy, text.replacen("revision = 1", "revision = 2", 1)).unwrap();
    });
    // THEN once the runner reads it, it visits the head again
    until(
        Duration::from_secs(60),
        "the runner never read main's move",
        || read_main(&test_box, &other, &tip),
    );
    until(
        Duration::from_secs(60),
        "the runner never visited the head again",
        || seen(&test_box, "a", &op).0 >= 2,
    );
    let _ = child.kill();
    let _ = child.wait();
}

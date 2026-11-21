//! G845 (a fleet finding, 2026-11-21: a head was checked alone for 954 s while nine changes
//! joined the line behind it, and the line's other runner checked nothing). A runner that did not
//! hold the head planned the head's batch from the line as it stood then, the head and all nine,
//! found each change it visited inside that batch, and checked nothing ahead. Now the holder's
//! lease names what its check covers, and the head alone when it checks alone, so the next batch
//! is planned on the main the head alone predicts, checked ahead, and lands on that record at its
//! turn. With footprint lanes, the batch checked ahead ends where its head will end it: at the
//! first member out of the head's lane.
//!
//! Guards to mutate, in `src/publish.rs`: `speculate_batch` reading `held_batch` (plan the head's
//! batch from the line instead), its `by_lane` (pass `false`), and `names_batch` before
//! `build_and_check` in `run_attempts` (name nothing for a head checked alone).

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::PRODUCER;
use support::queued::{Check, Held, OTHER_PRODUCER, finished, host};

const CONFIG: &str =
    "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 4\nbatch = { min = 2, max = 4 }\n";

const LANES: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 4\nfootprint_lanes = true\nbatch = { min = 2, max = 4 }\n";

/// The policy: check `ui` on `ui/`, and check `api` on `api/`.
fn lanes_policy() -> String {
    let check = |id: &str| {
        format!(
            "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"{id}/\"]\n"
        )
    };
    format!("revision = 1\n{}{}", check("ui"), check("api"))
}

/// Waits until the host's lease of `change` names `members` as what its holder's check covers.
fn wait_lease_names(held: &Held, change: &str, members: &[String]) {
    let name = format!(
        "{}:{}",
        bay::lease::lease_ref(change),
        bay::lease::LEASE_FILE
    );
    let start = Instant::now();
    loop {
        let shown = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&held.remote)
            .args(["show", &name])
            .output()
            .unwrap();
        if shown.status.success() {
            let lease: Value = serde_json::from_slice(&shown.stdout).unwrap();
            if lease["batch"] == json!(members) {
                return;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the lease of {change} never named {members:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The paths a check request names as changed, sorted.
fn changed(request: &Value) -> Vec<String> {
    let mut paths: Vec<String> = request["changed_paths"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|path| path.as_str().map(str::to_owned))
        .collect();
    paths.sort_unstable();
    paths
}

/// A host whose first check waits, with `one` queued on `dir` and held by runner A, alone in
/// the line, once its lease names it alone. Returns the host, `one`, A's pass, A's clone and
/// B's.
fn one_held_alone(
    config: &str,
    policy: Option<&str>,
    dir: &str,
) -> (Held, String, std::process::Child, PathBuf, PathBuf) {
    let seed = [("ui/a.rs", "fn a() {}\n"), ("api/a.rs", "fn a() {}\n")];
    let held = host(&Check {
        config,
        policy,
        seed: &seed,
        hold_first: true,
        fails: "false",
    });
    let one = held.queue("one", dir);
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let runner_a = held.spawn(&a);
    held.wait_held();
    wait_lease_names(&held, &one, &[format!("{one}.1")]);
    (held, one, runner_a, a, b)
}

#[test]
fn a_head_checked_alone_has_the_batch_that_joined_behind_it_checked_ahead() {
    // GIVEN runner A checks the head alone, as no batch formed, and its check waits
    let (held, one, runner_a, a, b) = one_held_alone(CONFIG, None, "ui");
    // WHEN two and three join the line behind it
    let two = held.queue("two", "ui");
    let three = held.queue("three", "ui");
    // THEN runner B checks two and three together, on the main one alone predicts
    let ahead = held.pass(&b);
    let requests = held.requests();
    assert_eq!(requests.len(), 2, "one held run, one ahead: {ahead}");
    assert_eq!(
        changed(&requests[1]),
        ["ui/three.rs", "ui/two.rs"],
        "{ahead}"
    );
    // AND once one lands, two and three land on that record, with no run of their own
    held.go();
    let first = finished(runner_a);
    let after = held.pass(&a);
    let landed = held.test_box.trunk_change_ids(&held.remote);
    for id in [&one, &two, &three] {
        assert!(landed.contains(id), "{id} did not land: {first} {after}");
    }
    assert_eq!(
        held.requests().len(),
        2,
        "the batch ran again at its turn: {after}"
    );
}

#[test]
fn with_footprint_lanes_the_batch_checked_ahead_ends_where_its_head_will_end_it() {
    // GIVEN footprint lanes, and runner A checks the head on ui/ alone
    let policy = lanes_policy();
    let (held, one, runner_a, a, b) = one_held_alone(LANES, Some(&policy), "ui");
    // WHEN two and three join on ui/, and four on api/
    let two = held.queue("two", "ui");
    let three = held.queue("three", "ui");
    let four = held.queue("four", "api");
    // THEN runner B checks two and three ahead, and not four, which is out of two's lane
    let ahead = held.pass(&b);
    let requests = held.requests();
    assert_eq!(requests.len(), 2, "one held run, one ahead: {ahead}");
    assert_eq!(
        changed(&requests[1]),
        ["ui/three.rs", "ui/two.rs"],
        "{ahead}"
    );
    // AND at their turn two and three land on that record, and four is checked alone
    held.go();
    let first = finished(runner_a);
    let after = held.pass(&a);
    let landed = held.test_box.trunk_change_ids(&held.remote);
    for id in [&one, &two, &three, &four] {
        assert!(landed.contains(id), "{id} did not land: {first} {after}");
    }
    let runs: Vec<Vec<String>> = held.requests().iter().map(changed).collect();
    assert_eq!(
        runs,
        [
            vec!["ui/one.rs".to_owned()],
            vec!["ui/three.rs".to_owned(), "ui/two.rs".to_owned()],
            vec!["api/four.rs".to_owned()],
        ],
        "{after}"
    );
}

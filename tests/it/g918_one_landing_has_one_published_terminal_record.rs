//! G918 (a fleet census, 2026-11-26: 510 of 11,186 published operations carried two published
//! terminal records, from two runners or from one runner twice, 0-191 s apart, so a reader that
//! counts records counts those landings twice; one second record said no claim and no attempt, as
//! a chain read before its lander's claim says): one landing has one published terminal record on
//! the remote's logs. A visit that finds a landing on main records it only with the push of its own
//! chain's landed record; when the remote refuses that chain, another process moved it, and the
//! record is that process's to write.
//!
//! The case keeps runner B's copy of the chain from before the landing, and B's reads of the
//! remote's chains and logs failing, while main still reads: B's claim finds the landing on main
//! on the chain it holds, and the remote refuses its chain's push.
//!
//! The second case keeps runner A's records apart from its publishing push, as a batch member's
//! go with its batch's one push, and B visits in that gap under a registry that signs landings,
//! so it cannot answer from the chain: it records the landing from main, and A's chain push is
//! refused after B's. On the previous release it left two records, A's and B's.
//!
//! The third case is that gap in a batch: A published two members and holds before their records,
//! which go in one push after the batch's publishing push, and B visits the members in that gap.
//! B finds each landing on main and records nothing, and A's push of the members' records lands.
//! On the previous release B left a record for each member, so each had two.
//!
//! Guard to mutate, in `src/publish.rs`: `Ctx::push_chain_with_record` answering `Riding::Apart` for a
//! chain the remote refused (the record goes alone, and the landing has two).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;

use support::queued::{Check, Held, OTHER_PRODUCER, Trust, host_under};
use support::{AGENT, EXECUTOR, PRODUCER, REGISTRAR, key_line, result_json};

/// A host whose first check waits for the test, so runner A holds the head while B reads it.
fn held() -> Held {
    host_under(
        &Check {
            config: "[runner]\nlease_s = 60\nheartbeat_s = 1\n",
            policy: None,
            seed: &[("src/a.rs", "fn a() {}\n")],
            hold_first: true,
            fails: "false",
        },
        Trust::Unsigned,
    )
}

/// The logs on the remote that hold a published terminal record of `op_id`, one entry a record.
fn published(held: &Held, op_id: &str) -> Vec<String> {
    let git = |args: &[&str]| held.test_box.git_bare(&held.remote, args);
    let mut found = Vec::new();
    for log in git(&["for-each-ref", "--format=%(refname)", "refs/bay/log/"]).lines() {
        for commit in git(&["log", "--format=%H", log]).lines() {
            let record: Value =
                serde_json::from_str(&git(&["show", &format!("{commit}:record.json")])).unwrap();
            if record["op_id"] == op_id && record["outcome"] == "published" {
                found.push(log.to_owned());
            }
        }
    }
    found
}

#[test]
fn a_visit_that_finds_a_landing_on_a_chain_it_could_not_read_again_records_nothing() {
    let rig = held();
    let head = rig.queue("head", "src");
    let op = format!("{head}.1");
    let first = rig.executor("exec-a", PRODUCER);
    let second = rig.executor("exec-b", OTHER_PRODUCER);
    rig.test_box
        .git_ok(&second, &["config", "bay.runnerName", "exec-b"]);

    // GIVEN runner B read the head's chain while runner A held it in its check
    let holder = rig.spawn(&first);
    rig.wait_held();
    let (code, pass) = result_json(rig.test_box.bay(&second).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{pass}");
    // AND A landed the head, its terminal record riding with its publishing push
    rig.go();
    let output = holder.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        rig.test_box.trunk_change_ids(&rig.remote).contains(&head),
        "runner A did not land the head"
    );
    assert_eq!(published(&rig, &op).len(), 1, "A's landing");

    // WHEN runner B visits the head while the remote's chains and logs cannot be read
    let dark = rig.test_box.path().join("dark");
    rig.test_box.wrap_git(&format!(
        "if [ -e '{dark}' ]; then\n  case \" $* \" in\n    *\" fetch \"*refs/bay/changes/*|*\" fetch \"*refs/bay/log/*)\n      echo 'fatal: unable to access the remote' >&2\n      exit 128\n      ;;\n  esac\nfi",
        dark = dark.display()
    ));
    std::fs::write(&dark, "").unwrap();
    let (code, visit) = result_json(rig.test_box.bay(&second).args(["runner", "run", "--once"]));
    std::fs::remove_file(&dark).unwrap();
    assert_eq!(code, 0, "{visit}");
    // AND each runner passes once more, so no record it holds stays on its box
    for clone in [&first, &second] {
        let (code, pass) = result_json(rig.test_box.bay(clone).args(["runner", "run", "--once"]));
        assert_eq!(code, 0, "{pass}");
    }

    // THEN the landing has one published terminal record: its lander's, never B's
    let logs = published(&rig, &op);
    assert_eq!(logs.len(), 1, "{logs:?}; B's visit: {visit}");
    assert!(!logs[0].ends_with("exec-b"), "{logs:?}");
}

/// Runners that hold a lease 2 s and land one change at a time.
const SINGLE: &str = "[runner]\nlease_s = 2\nheartbeat_s = 1\n";

/// Runners that hold a lease 2 s and land two or more changes in one batch.
const BATCH: &str = "[runner]\nlease_s = 2\nheartbeat_s = 1\nbatch = { min = 2, max = 8, max_failure_rate = 1.0 }\n";

/// A host that signs every record, whose runners run `config` with no check held.
fn signed(config: &'static str) -> Held {
    host_under(
        &Check {
            config,
            policy: None,
            seed: &[("src/a.rs", "fn a() {}\n")],
            hold_first: false,
            fails: "false",
        },
        Trust::Signed,
    )
}

/// Enrolls runner B under a key of its own, so its records can reach the remote; the key's path.
fn enroll_runner_b(rig: &Held) -> PathBuf {
    let key = rig.test_box.path().join("keys").join("runner-b");
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "runner-b", "-f"])
        .arg(&key)
        .status()
        .unwrap();
    assert!(made.success());
    rig.test_box.enroll_files(
        &rig.repo,
        &rig.keys,
        &[
            ("agent", key_line(AGENT, &rig.keys.agent)),
            (
                "runner",
                key_line(EXECUTOR, &rig.keys.executor) + &key_line("exec-b", &key),
            ),
            (
                "producer",
                key_line(PRODUCER, &rig.keys.producer) + &key_line(OTHER_PRODUCER, &rig.keys.rogue),
            ),
            ("registry", key_line(REGISTRAR, &rig.keys.registry)),
        ],
    );
    key
}

/// Runner B's clone, signing with `key`. It holds the registry, as a runner that has landed
/// before holds it: a pass reads the local registry before it fetches one.
fn runner_b(rig: &Held, key: &Path) -> PathBuf {
    let clone = rig.executor("exec-b", OTHER_PRODUCER);
    rig.test_box
        .git_ok(&clone, &["config", "bay.runnerKey", key.to_str().unwrap()]);
    rig.test_box
        .git_ok(&clone, &["config", "bay.runnerName", "exec-b"]);
    rig.test_box.git_ok(
        &clone,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/registry:refs/bay/registry",
        ],
    );
    clone
}

/// Runner A's pass, its records kept apart from its publishing push and held after that push
/// until the returned file exists.
fn land_apart_and_hold(rig: &Held, clone: &Path) -> (std::process::Child, PathBuf) {
    let pause = rig.test_box.path().join("pause");
    let lander = rig
        .test_box
        .bay_process(clone)
        .args(["--json", "runner", "run", "--once"])
        .env("BAY_TEST_LAND_APART", "1")
        .env("BAY_TEST_PAUSE_AT", "after-publish")
        .env("BAY_TEST_PAUSE_FILE", &pause)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let paused = rig.test_box.path().join("pause.paused");
    let began = Instant::now();
    while !paused.exists() {
        assert!(
            began.elapsed() < Duration::from_secs(120),
            "runner A never reached its publishing push"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    (lander, pause)
}

/// Lets runner A go on from its hold, and waits for its pass to succeed.
fn go_on(lander: std::process::Child, pause: &Path) {
    std::fs::write(pause, "").unwrap();
    let output = lander.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The operations of `ids` that `pass` ran.
fn visited(pass: &Value, ids: &[String]) -> Vec<String> {
    let ran = pass["detail"]["ran"].as_array().into_iter().flatten();
    let ops: Vec<String> = ran
        .filter_map(|entry| entry["op_id"].as_str().map(str::to_owned))
        .collect();
    ids.iter()
        .map(|id| format!("{id}.1"))
        .filter(|op| ops.contains(op))
        .collect()
}

/// A visit in the gap between a signed landing's publishing push and its records: under a registry
/// that signs landings the visitor cannot answer from the chain, and it must still record nothing.
/// The lander's records go after the gap, as a batch member's go with its batch's one push.
#[test]
fn a_visit_while_a_signed_landing_keeps_its_records_apart_records_nothing() {
    let rig = signed(SINGLE);
    let key = enroll_runner_b(&rig);
    let head = rig.queue("head", "src");
    let op = format!("{head}.1");
    let first = rig.executor("exec-a", PRODUCER);
    let second = runner_b(&rig, &key);

    // GIVEN runner A published the head and holds at its publishing push, its records kept apart
    // from that push, as a batch's landing keeps them
    let (lander, pause) = land_apart_and_hold(&rig, &first);
    assert!(
        rig.test_box.trunk_change_ids(&rig.remote).contains(&head),
        "runner A did not publish the head"
    );

    // WHEN runner B visits the head after A's lease ran out
    std::thread::sleep(std::time::Duration::from_secs(3));
    let (code, visit) = result_json(rig.test_box.bay(&second).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{visit}");
    let visited = !visited(&visit, std::slice::from_ref(&head)).is_empty();
    // AND A goes on and pushes its records
    go_on(lander, &pause);
    // AND each runner passes once more, so no record it holds stays on its box
    for clone in [&first, &second] {
        let (code, pass) = result_json(rig.test_box.bay(clone).args(["runner", "run", "--once"]));
        assert_eq!(code, 0, "{pass}");
    }

    // THEN B visited the head in that gap
    assert!(visited, "runner B never visited the head: {visit}");
    // AND the landing has one published terminal record
    let logs = published(&rig, &op);
    assert_eq!(logs.len(), 1, "{logs:?}; B's visit: {visit}");
}

/// A visit in the gap between a signed batch's publishing push and its members' records: the
/// visitor records a member from main, so the batch's one push of its records is refused, and
/// each member's chain goes alone. A member must still have one published terminal record.
#[test]
fn a_visit_while_a_signed_batch_keeps_its_records_apart_records_nothing() {
    let rig = signed(BATCH);
    let key = enroll_runner_b(&rig);
    let ids: Vec<String> = ["b1", "b2"]
        .into_iter()
        .map(|name| rig.queue(name, "src"))
        .collect();
    let first = rig.executor("exec-a", PRODUCER);
    let second = runner_b(&rig, &key);

    // GIVEN runner A published both members in one batch and holds at its publishing push, their
    // records kept apart from that push
    let (lander, pause) = land_apart_and_hold(&rig, &first);
    let trunk = rig.test_box.trunk_change_ids(&rig.remote);
    assert!(
        ids.iter().all(|id| trunk.contains(id)),
        "runner A did not publish the batch"
    );

    // WHEN runner B visits the members after A's leases ran out
    std::thread::sleep(Duration::from_secs(3));
    let (code, visit) = result_json(rig.test_box.bay(&second).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{visit}");
    let seen = visited(&visit, &ids);
    // AND A goes on and pushes its members' records
    go_on(lander, &pause);
    // AND each runner passes once more, so no record it holds stays on its box
    for clone in [&first, &second] {
        let (code, pass) = result_json(rig.test_box.bay(clone).args(["runner", "run", "--once"]));
        assert_eq!(code, 0, "{pass}");
    }

    // THEN B visited a member in that gap
    assert!(!seen.is_empty(), "runner B never visited a member: {visit}");
    // AND each member has one published terminal record
    for id in &ids {
        let logs = published(&rig, &format!("{id}.1"));
        assert_eq!(logs.len(), 1, "{id}: {logs:?}; B's visit: {visit}");
    }
}

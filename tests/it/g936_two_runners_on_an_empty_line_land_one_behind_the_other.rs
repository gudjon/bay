//! G936 (BACKLOG 349; a load test, 2026-11-27: with the line empty, two runners each published a
//! different change free of the line on one main; the second landing push was refused and its
//! change checked again, twelve times in a row in one minute, each runner losing to the other). A
//! change in an empty line published free, beside the line, so a second runner that took another
//! change meanwhile built on the same main and lost the push. Now, with another runner live, a
//! change takes a place even in an empty line: a change that comes while it runs waits behind it,
//! is checked ahead on the main the head predicts, as a batch of one, and lands on that record at
//! its turn. A runner that comes while a change runs free, before its runner read the newcomer's
//! presence, gives that change the place, and the change's runner takes it when the host refuses
//! its push free of it.
//!
//! Guards to mutate, in `src/publish.rs`: `races_free` (answer `false`: the second change
//! publishes free, its push or the first's is refused, and one is checked again),
//! `next_to_head` in `speculate_batch` (answer `false`: the change behind the head is screened
//! alone on trunk, and checked again at its turn), and `placed_meanwhile` (answer `None`: the
//! first change's push, free of the place another runner gave it, fails, and both are checked
//! again together).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::queued::{Check, Held, OTHER_PRODUCER, finished, host};
use support::{AGENT, EXECUTOR, PRODUCER, REGISTRAR, key_line, result_json};

/// Runner B's name, which the registry enrolls beside the host's own runner.
const RUNNER_B: &str = "exec-b";

const CONFIG: &str =
    "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 4\nbatch = { min = 2, max = 4 }\n";

/// A host whose first check waits, seeded with `ui/a.rs`, and runners A and B in clones of their
/// own. Each clone runs as the registry's one runner unless told otherwise; B runs as a runner of
/// its own, enrolled under its own key, so each runner reads the other's presence.
fn two_runners() -> (Held, PathBuf, PathBuf) {
    let seed = [("ui/a.rs", "fn a() {}\n")];
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &seed,
        hold_first: true,
        fails: "false",
    });
    let keys = &held.keys;
    let key_b = runner_key(&held);
    held.test_box.enroll_files(
        &held.repo,
        keys,
        &[
            ("agent", key_line(AGENT, &keys.agent)),
            (
                "runner",
                key_line(EXECUTOR, &keys.executor) + &key_line(RUNNER_B, &key_b),
            ),
            (
                "producer",
                key_line(PRODUCER, &keys.producer) + &key_line(OTHER_PRODUCER, &keys.rogue),
            ),
            ("registry", key_line(REGISTRAR, &keys.registry)),
        ],
    );
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    for (key, value) in [
        ("bay.runnerKey", key_b.to_str().unwrap()),
        ("bay.runnerName", RUNNER_B),
    ] {
        held.test_box.git_ok(&b, &["config", key, value]);
    }
    (held, a, b)
}

/// A new key for runner B, which no other class or principal holds.
fn runner_key(held: &Held) -> PathBuf {
    let path = held.test_box.path().join("keys").join(RUNNER_B);
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", RUNNER_B, "-f"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(made.success());
    path
}

/// Starts change `file` on `<dir>/`, adds `<dir>/<file>.rs` in its view and submits it, leaving
/// its place to a runner. Returns its change id.
fn submitted(held: &Held, file: &str, dir: &str) -> String {
    let paths = format!("{dir}/");
    let started = held
        .test_box
        .bay_ok(&held.repo, &["start", file, "--paths", paths.as_str()]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let full = view.join(format!("{dir}/{file}.rs"));
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, format!("fn {file}() {{}}\n")).unwrap();
    let (code, result) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{result}");
    id
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

/// While one is checked, two is submitted and runner B takes it: B checks two ahead, alone, on
/// the main one predicts, and publishes nothing. Then one's check goes, A lands one, and A's next
/// pass lands two on its record, with no check of its own and no push left refused. Returns B's
/// pass.
fn two_lands_behind_one(
    held: &Held,
    a: &Path,
    b: &Path,
    one: &str,
    runner_a: std::process::Child,
) -> Value {
    let two = submitted(held, "two", "ui");
    let beside = held.pass(b);
    let runs: Vec<Vec<String>> = held.requests().iter().map(changed).collect();
    assert_eq!(
        runs,
        [vec!["ui/one.rs".to_owned()], vec!["ui/two.rs".to_owned()]],
        "{beside}"
    );
    let landed = held.test_box.trunk_change_ids(&held.remote);
    assert!(!landed.contains(&two), "two published beside one: {beside}");
    held.go();
    let first = finished(runner_a);
    let after = held.pass(a);
    let landed = held.test_box.trunk_change_ids(&held.remote);
    for id in [one, two.as_str()] {
        assert!(
            landed.iter().any(|it| it == id),
            "{id} did not land: {first} {after}"
        );
    }
    assert_eq!(
        held.requests().len(),
        2,
        "a change was checked again: {first} {after}"
    );
    for pass in [&first, &after] {
        assert!(!pass.to_string().contains("refused"), "{pass}");
    }
    beside
}

#[test]
fn two_runners_on_an_empty_line_land_two_changes_one_behind_the_other_with_one_check_each() {
    // GIVEN two runners on one host, B live, and an empty line
    let (held, a, b) = two_runners();
    let idle = held.pass(&b);
    assert_eq!(idle["detail"]["presence"]["pushed"], true, "{idle}");
    // WHEN one is submitted, and runner A takes it and checks it, its check waiting
    let one = submitted(&held, "one", "ui");
    let runner_a = held.spawn(&a);
    held.wait_held();
    // THEN two, submitted meanwhile, is checked ahead behind it and lands on its record
    let beside = two_lands_behind_one(&held, &a, &b, &one, runner_a);
    assert!(!beside.to_string().contains("refused"), "{beside}");
}

#[test]
fn a_change_another_runner_placed_while_it_ran_free_lands_from_that_place() {
    // GIVEN two runners on one host, B not yet live, and an empty line
    let (held, a, b) = two_runners();
    // WHEN one is submitted, and runner A takes it free of the line and checks it, its check
    // waiting
    let one = submitted(&held, "one", "ui");
    let runner_a = held.spawn(&a);
    held.wait_held();
    // THEN B, live now, gives one a place, checks two ahead behind it, and two lands on one's
    // record once A lands one from that place
    two_lands_behind_one(&held, &a, &b, &one, runner_a);
}

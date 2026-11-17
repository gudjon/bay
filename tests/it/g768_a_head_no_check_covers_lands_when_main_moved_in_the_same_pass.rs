//! G768 (a fleet measurement, 2026-11-17: a head that yielded "main moved" waited 277 s at the
//! median for its next pass, and that pass ran no check on the new main: of 33 such merges in
//! three hours one ran a check after the move, and 7,301 s were the wait). A head no pre-merge
//! check covers on the new main is built again and lands in the same visit when main moved
//! under its publishing push; a head whose check must run again yields as before.
//!
//! Guard to mutate: in `src/publish.rs` `after_trunk_moved`, drop the `needs_no_check` branch,
//! and the head yields and waits for a later pass.

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use support::queued::{Check, Held, finished, host, ran_for};
use support::{EXECUTOR, PRODUCER, result_json};

const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"src/**\"]\n";

const CONFIG: &str = "[runner]\nlease_s = 600\nheartbeat_s = 30\n";

/// Starts change `intent` on `dir/`, writes a file there, and submits it for a runner, which a
/// change no check covers is not by default (G657: its own merge pushes it), and gives it a
/// place in line. Returns its id and view.
fn submit_for_runner(held: &Held, intent: &str, dir: &str) -> (String, PathBuf) {
    let started = held.test_box.bay_ok(
        &held.repo,
        &["start", intent, "--paths", &format!("{dir}/")],
    );
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::create_dir_all(view.join(dir)).unwrap();
    std::fs::write(view.join(format!("{dir}/{intent}.md")), "new\n").unwrap();
    let (code, queued) = result_json(
        held.test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    let signer = bay::signing::Signer {
        key: held.keys.executor.clone(),
        name: EXECUTOR.to_owned(),
    };
    bay::queue::join(&opened, Some(&signer), &format!("{id}.1"), &id).unwrap();
    (id, view)
}

/// Queues `head` (on `head_dir`), holds a runner just before it publishes the head, lands an
/// unrelated documents change past it by its author's own merge (G552), and lets the runner go
/// on. Returns the runner's pass, the head's id and the head's chain subjects.
fn main_moves_under(head_dir: &str) -> (serde_json::Value, String, String) {
    let held = host(&Check {
        config: CONFIG,
        policy: Some(POLICY),
        seed: &[("src/a.rs", "fn a() {}\n"), ("docs/old.md", "old\n")],
        hold_first: false,
        fails: "false",
    });
    let (queued_head, _) = submit_for_runner(&held, "head", head_dir);
    let runner = held.executor("exec-a", PRODUCER);
    let release = held.test_box.path().join("release-a");
    let pass = held
        .test_box
        .bay_process(&runner)
        .env("BAY_TEST_PAUSE_AT", "before-publish")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .env("BAY_TEST_PAUSE_TIMEOUT_S", "900")
        .args(["--json", "runner", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = PathBuf::from(format!("{}.paused", release.display()));
    let deadline = Instant::now() + Duration::from_secs(120);
    while !paused.exists() {
        assert!(
            Instant::now() < deadline,
            "the head never came to its publish"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Main moves under the head: a documents change no check covers, holding a place behind
    // it, passes it by its author's own merge (G552), with no runner visiting the head.
    let (passer, view) = submit_for_runner(&held, "passer", "notes");
    let (_, moved) = result_json(held.test_box.bay(&view).args(["merge", "--wait"]));
    assert!(
        held.test_box
            .trunk_change_ids(&held.remote)
            .contains(&passer),
        "the passer did not land: {moved}"
    );
    std::fs::write(&release, "go").unwrap();
    let pass = finished(pass);
    let chain = held.test_box.git_bare(
        &held.remote,
        &[
            "log",
            "--format=%s",
            &format!("refs/bay/changes/{queued_head}"),
        ],
    );
    let _ = ran_for(&pass, &queued_head);
    (pass, queued_head, chain)
}

#[test]
fn a_head_no_check_covers_lands_in_the_same_pass_when_main_moved() {
    // WHEN main moves under the publishing push of a head no check covers
    let (pass, queued_head, chain) = main_moves_under("docs");

    // THEN it lands in that same pass, without yielding its place
    assert_eq!(
        ran_for(&pass, &queued_head)["decision"],
        "merged",
        "{pass}\n{chain}"
    );
    assert!(!chain.contains("yielded"), "{chain}");
}

#[test]
fn a_head_whose_check_must_run_again_yields_when_main_moved() {
    // WHEN main moves under the publishing push of a head its check covers
    let (pass, queued_head, chain) = main_moves_under("src");

    // THEN it yields its pass, as before, keeping its place for the next
    assert!(chain.contains("yielded"), "{chain}\n{pass}");
    assert_eq!(ran_for(&pass, &queued_head)["decision"], "queued", "{pass}");
}

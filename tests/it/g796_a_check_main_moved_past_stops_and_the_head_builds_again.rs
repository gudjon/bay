//! G796 (a fleet measurement, 2026-11-19: in one day 124 heads ran 5.5 h of checks on
//! candidates main had already moved past, and learned it only when their merging push was
//! refused; one 4-minute check went on 224 s after a change that passed its head landed 28 s
//! into it). A runner looks at main once a heartbeat while a candidate is checked. Once main
//! moved past the candidate's base, the check stops (its adapter is killed, and it keeps no
//! record), and the head builds again on the new main in the same visit: it lands on a check
//! of that candidate, never on the stopped one.
//!
//! Guards to mutate, in `src/publish.rs`:
//! - `watch_main` returning `None` (the head's check runs on after main moved: the visit ends
//!   only when the test lets the check go);
//! - `after_stopped_check` leaving the move off the chain (G552's test fails: another runner
//!   reads no move under the head, and a second document lands while the head checks again).

#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, finished, host, ran_for};
use support::{check_files, passing_result};

#[test]
fn a_check_main_moved_past_stops_and_the_head_lands_on_the_new_main() {
    // One pre-merge check, required only for src/: a document needs none, and passes the head.
    let policy = check_files("x", &passing_result(1), 1, 120)
        .into_iter()
        .find(|(path, _)| path == ".bay/policy.toml")
        .map(|(_, text)| text.replacen("timeout_s", "when_paths = [\"src/**\"]\ntimeout_s", 1))
        .unwrap();
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 4\n",
        policy: Some(&policy),
        seed: &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
        hold_first: true,
        fails: "false",
    });
    let slow = held.queue("slow", "src");
    let doc = held.queue_with("doc", &["docs"], &[("docs/doc.md", "doc\n")]);
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    // The head's check is short of the hold window (G791), so the document may pass it.
    held.test_box
        .git_ok(&b, &["config", "bay.testPasserHoldS", "3600"]);

    // The head's first check runs, and holds until the test lets it go.
    let mut runner_a = held.spawn(&a);
    held.wait_held();
    // The document lands past it: main moves under the head's candidate.
    let pass = held.pass(&b);
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&doc),
        "the document did not pass the head: {pass}"
    );
    let moved = held.test_box.git_bare(&held.remote, &["rev-parse", "main"]);

    // The head's runner sees the move within a few heartbeats, stops the held check, and lands
    // the head on a check of a candidate built on the new main. The test never lets the first
    // check go while the runner may still end on its own.
    let began = Instant::now();
    let ended = loop {
        if runner_a.try_wait().unwrap().is_some() {
            break true;
        }
        if began.elapsed() > Duration::from_secs(60) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if !ended {
        held.go();
    }
    let ran = finished(runner_a);
    assert!(
        ended,
        "the head's check ran on after main moved past its candidate: {ran}"
    );
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&slow),
        "the head did not land: {ran}"
    );
    assert_eq!(
        ran_for(&ran, &slow)["decision"],
        "merged",
        "the head did not land in the visit that stopped its check: {ran}"
    );
    // The candidate built again is a new candidate: the stopped one named its scratch and its
    // records.
    let candidate = held.test_box.git_bare(
        &held.remote,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Candidate-Id,valueonly)",
            "main",
        ],
    );
    assert_eq!(candidate.trim(), format!("{slow}.1.2"), "{ran}");

    // Two runs: the stopped one on the old base, and the one the head landed on, on the new
    // main. The stopped run keeps no record.
    let requests = held.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_ne!(
        requests[0]["base"]["commit"],
        moved.as_str(),
        "{requests:?}"
    );
    assert_eq!(
        requests[1]["base"]["commit"],
        moved.as_str(),
        "{requests:?}"
    );
    let stopped_tree = requests[0]["candidate"]["tree"].as_str().unwrap();
    let notes = held.test_box.git_bare(
        &held.remote,
        &[
            "notes",
            &format!("--ref=refs/notes/bay/evidence/{PRODUCER}"),
            "list",
        ],
    );
    let stopped_note = held
        .test_box
        .git_bare(&held.remote, &["rev-parse", stopped_tree]);
    assert!(
        !notes.contains(&stopped_note),
        "the stopped check left a record: {notes}"
    );
    held.go();
}

#[test]
fn a_visit_after_a_stopped_candidate_never_reuses_a_candidate_number() {
    let test_box = support::TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let dir = test_box.path().to_path_buf();
    // The check's first run waits for the test; later runs pass at once.
    let count = test_box.relay(&format!(
        "printf x >> '{runs}'; n=$(wc -c < '{runs}' | tr -d ' '); if [ \"$n\" -eq 1 ]; then : > '{held}'; until [ -e '{go}' ] || ! kill -0 $PPID 2>/dev/null; do sleep 0.05; done; fi; echo \"$n\"",
        runs = dir.join("runs").display(),
        held = dir.join("held").display(),
        go = dir.join("go").display(),
    ));
    let files: Vec<(String, String)> =
        check_files(&format!("n=$({count})\n"), &passing_result(1), 1, 120)
            .into_iter()
            .chain([(
                ".bay/config.toml".to_owned(),
                "[runner]\nlease_s = 30\nheartbeat_s = 1\n".to_owned(),
            )])
            .collect();
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/a.md", "a\n")],
        &files,
    );
    let outside = test_box.clone_of(&remote, "mover");
    let mut count = 0;
    let mut move_main = || {
        count += 1;
        test_box.git_ok(&outside, &["pull", "-q", "--rebase"]);
        std::fs::write(outside.join(format!("docs/move-{count}.md")), "moved\n").unwrap();
        test_box.git_ok(&outside, &["add", "docs"]);
        test_box.git_ok(
            &outside,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "moved",
            ],
        );
        test_box.git_ok(&outside, &["push", "-q", "origin", "main"]);
    };
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = support::result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    let runner = test_box.clone_of(&remote, "runner");

    // GIVEN the runner stops the first candidate's check when main moves under it, and holds
    // before the merging push of the second, checked and persisted
    let publish = dir.join("publish");
    let first = test_box
        .bay_process(&runner)
        .args(["--json", "runner", "run", "--once"])
        .env("BAY_TEST_PAUSE_AT", "before-publish")
        .env("BAY_TEST_PAUSE_FILE", &publish)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let began = Instant::now();
    while !dir.join("held").exists() {
        assert!(began.elapsed() < Duration::from_secs(120), "no check ran");
        std::thread::sleep(Duration::from_millis(50));
    }
    move_main();
    let paused = publish.with_extension("paused");
    while !paused.exists() {
        assert!(
            began.elapsed() < Duration::from_secs(120),
            "the runner never reached its merging push"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // WHEN main moves before that push, so the push is refused
    move_main();
    std::fs::write(&publish, "go").unwrap();
    let _ = finished(first);
    assert!(
        !test_box.trunk_change_ids(&remote).contains(&id),
        "the change landed on a main that moved"
    );

    // THEN the runner's next visit builds a candidate of a number no earlier one had, and lands it
    let ran = test_box.bay_ok(&runner, &["runner", "run", "--once"]);
    assert!(test_box.trunk_change_ids(&remote).contains(&id), "{ran}");
    let candidate = test_box.git_bare(
        &remote,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Candidate-Id,valueonly)",
            "main",
        ],
    );
    assert_eq!(candidate.trim(), format!("{id}.1.3"), "{ran}");
    std::fs::write(dir.join("go"), "").unwrap();
}

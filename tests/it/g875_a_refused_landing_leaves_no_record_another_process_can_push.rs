//! G875 (a fleet finding, 2026-11-23: bay answered a change merged whose commit main never held).
//! A landing wrote its chain's landed record and its terminal record on this box's refs, then
//! pushed them with main. Every bay process on a box shares those refs, and another process's
//! sync of offline work pushed the chain's record ahead of the landing's own push, which main's
//! move then refused. The remote said landed; runners and the author's wait answered merged.
//!
//! A landing's records now stay off the box's refs until its push takes them, and a landing read
//! from a chain alone, by the author's `bay merge` or by a looping runner that read the change
//! held, is answered merged only once main holds its commit.
//!
//! Guards to mutate: in `src/publish.rs`, `mark_landed` makes its record with `save_riding`
//! (use `save_cas`, and the chain ref holds the landing before its push); `landed_on_chain` and
//! `landed_if_on_main` ask `main_holds` (drop either, and a phantom landing is answered merged);
//! in `src/log.rs`, `ride` moves the log ref only after the push (move it first, and the
//! box's log holds the terminal record before the push).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;

use support::PRODUCER;
use support::queued::{Check, OTHER_PRODUCER, Trust, host_under, ran_for};
use support::{TestBox, check_files, passing_result, result_json};

/// The terminal records of `op_id` with outcome `published` on every log `git` reads.
fn published_records(git: &dyn Fn(&[&str]) -> String, op_id: &str) -> usize {
    git(&["for-each-ref", "--format=%(refname)", "refs/bay/log/"])
        .lines()
        .flat_map(|log| {
            git(&["log", "--format=%H", log])
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|commit| {
            let record: Value =
                serde_json::from_str(&git(&["show", &format!("{commit}:record.json")])).unwrap();
            record["op_id"] == op_id && record["outcome"] == "published"
        })
        .count()
}

/// The change record of `id` as `git` reads it.
fn chain(git: &dyn Fn(&[&str]) -> String, id: &str) -> Value {
    serde_json::from_str(&git(&[
        "show",
        &format!("refs/bay/changes/{id}:change.json"),
    ]))
    .unwrap()
}

/// Whether the remote's main holds change `id`.
fn on_main(test_box: &TestBox, remote: &Path, id: &str) -> bool {
    test_box
        .trunk_change_ids(remote)
        .iter()
        .any(|landed| landed == id)
}

/// A repository with one passing check, a change queued in it, and its view.
fn queued_change() -> (
    TestBox,
    std::path::PathBuf,
    std::path::PathBuf,
    String,
    std::path::PathBuf,
) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    (test_box, repo, remote, id, view)
}

/// Waits for the process that writes `<release>.paused` to reach its pause.
fn wait_paused(release: &Path) {
    let paused = release.with_extension("paused");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !paused.exists() {
        assert!(
            Instant::now() < deadline,
            "the runner never reached its pause"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_landing_keeps_its_records_off_the_box_until_its_push_takes_them() {
    let (test_box, repo, remote, id, _view) = queued_change();
    let op_id = format!("{id}.1");

    // GIVEN runner E1 held after it made the landing's records, before its push
    let e1 = test_box.clone_of(&remote, "e1");
    test_box.git_ok(&e1, &["config", "bay.producerName", "e1-checks"]);
    let release = test_box.path().join("release-e1");
    let child = test_box
        .bay_process(&e1)
        .env("BAY_TEST_PAUSE_AT", "landing-marked")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["--json", "runner", "run", "--once"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    wait_paused(&release);

    // THEN the box's refs, which every bay process on it shares, hold no landing yet
    let local = |args: &[&str]| test_box.git_ok(&e1, args);
    let held = chain(&local, &id);
    assert_ne!(held["state"], "landed", "{held}");
    assert_eq!(published_records(&local, &op_id), 0);

    // WHEN another bay process on that box syncs its offline work, and main moves
    let (code, status) = result_json(test_box.bay(&e1).arg("status"));
    assert_eq!(code, 0, "{status}");
    let remote_git = |args: &[&str]| test_box.git_bare(&remote, args);
    let synced = chain(&remote_git, &id);
    assert_ne!(
        synced["state"], "landed",
        "a sync pushed the landing: {synced}"
    );
    assert_eq!(published_records(&remote_git, &op_id), 0);
    test_box.seed_trunk(&repo, &[("src/other.rs", "fn other() {}\n")]);
    std::fs::write(&release, "go").unwrap();
    let output = child.wait_with_output().unwrap();
    let ran: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{ran}");

    // THEN no record says landed while main lacks the change, and E1 answers merged only once
    // main holds it
    let landed = on_main(&test_box, &remote, &id);
    if ran_for(&ran, &id)["decision"] == "merged" {
        assert!(landed, "answered merged, main lacks it: {ran}");
    }
    let record = chain(&remote_git, &id);
    if !landed {
        assert_ne!(record["state"], "landed", "{record}");
        assert_eq!(published_records(&remote_git, &op_id), 0, "{ran}");
    }

    // AND a later pass lands it, once
    if !landed {
        let e2 = test_box.clone_of(&remote, "e2");
        test_box.git_ok(&e2, &["config", "bay.producerName", "e2-checks"]);
        for _ in 0..3 {
            let (code, ran) = result_json(test_box.bay(&e2).args(["runner", "run", "--once"]));
            assert_eq!(code, 0, "{ran}");
            if on_main(&test_box, &remote, &id) {
                break;
            }
        }
    }
    assert_eq!(
        test_box
            .trunk_change_ids(&remote)
            .iter()
            .filter(|landed| **landed == id)
            .count(),
        1
    );
    assert_eq!(chain(&remote_git, &id)["state"], "landed");
}

/// Runs git in `dir` with `input` on its stdin: its stdout, trimmed.
fn git_with_input(test_box: &TestBox, dir: &Path, args: &[&str], input: &str) -> String {
    let mut child = test_box
        .git(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Writes on the remote chain of `id` the record a client before G875 could leave: the change
/// landed and its last operation published on its checked candidate, which main never took.
fn phantom_landing(test_box: &TestBox, remote: &Path, id: &str) {
    let forger = test_box.clone_of(remote, "forger");
    let name = format!("refs/bay/changes/{id}");
    test_box.git_ok(
        &forger,
        &["fetch", "-q", "origin", &format!("+{name}:{name}")],
    );
    let git = |args: &[&str]| test_box.git_ok(&forger, args);
    let head = git(&["rev-parse", &name]);
    let draft = git(&["rev-parse", &format!("{head}^2")]);
    let mut record = chain(&git, id);
    record["state"] = Value::from("landed");
    let op = record["operations"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    op["state"] = Value::from("published");
    op["outcome"] = Value::Null;
    let candidate = op["attempts"].as_array().unwrap().last().unwrap()["commit"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut json = serde_json::to_string_pretty(&record).unwrap();
    json.push('\n');
    let blob = git_with_input(test_box, &forger, &["hash-object", "-w", "--stdin"], &json);
    let tree = git_with_input(
        test_box,
        &forger,
        &["mktree"],
        &format!("100644 blob {blob}\tchange.json\n"),
    );
    let commit = git(&[
        "commit-tree",
        &tree,
        "-p",
        &head,
        "-p",
        &draft,
        "-p",
        &candidate,
        "-m",
        &format!("landed {id}.1"),
    ]);
    git(&["push", "-q", "origin", &format!("{commit}:{name}")]);
}

#[test]
fn a_chain_that_says_landed_while_main_lacks_the_commit_is_never_answered_merged() {
    let (test_box, _repo, remote, id, view) = queued_change();
    let op_id = format!("{id}.1");

    // GIVEN a runner that ended after it made the landing's records, before its push
    let e1 = test_box.clone_of(&remote, "e1");
    test_box.git_ok(&e1, &["config", "bay.producerName", "e1-checks"]);
    let out = test_box
        .bay_process(&e1)
        .env("BAY_TEST_KILL_AT", "landing-marked")
        .args(["--json", "runner", "run", "--once"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(137), "{out:?}");
    assert!(!on_main(&test_box, &remote, &id));
    // AND runner E2 held as its visit to the change begins
    let e2 = test_box.clone_of(&remote, "e2");
    test_box.git_ok(&e2, &["config", "bay.producerName", "e2-checks"]);
    let release = test_box.path().join("release-e2");
    let child = test_box
        .bay_process(&e2)
        .env("BAY_TEST_PAUSE_AT", "visit-begins")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["--json", "runner", "run", "--once"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    wait_paused(&release);
    // AND a chain on the remote that says the change landed, as a client before G875 left one
    phantom_landing(&test_box, &remote, &id);
    let remote_git = |args: &[&str]| test_box.git_bare(&remote, args);

    // WHEN E2 visits it
    std::fs::write(&release, "go").unwrap();
    let output = child.wait_with_output().unwrap();
    let ran: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{ran}");
    // THEN it is not answered merged, and no record of a landing is written
    assert_ne!(ran_for(&ran, &id)["decision"], "merged", "{ran}");
    assert!(!on_main(&test_box, &remote, &id));
    assert_eq!(published_records(&remote_git, &op_id), 0, "{ran}");

    // WHEN its author asks for the outcome
    let (_, merged) = result_json(test_box.bay(&view).arg("merge"));
    // THEN it is not answered merged either
    assert_ne!(merged["decision"], "merged", "{merged}");
    assert_eq!(merged["reason"], "MERGE_OUTCOME_UNKNOWN", "{merged}");
    assert_eq!(published_records(&remote_git, &op_id), 0, "{merged}");
}

/// Runner B, looping, in `clone`, held at the start of its first visit until `go` exists, its
/// journal to `journal`.
fn held_looping_runner(
    test_box: &TestBox,
    clone: &Path,
    go: &Path,
    journal: &Path,
) -> std::process::Child {
    let child = test_box
        .bay_process(clone)
        .args(["--json", "runner", "run", "--interval-s", "1"])
        .env("BAY_TEST_PAUSE_AT", "visit-begins")
        .env("BAY_TEST_PAUSE_FILE", go)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(journal).unwrap())
        .spawn()
        .unwrap();
    wait_paused(go);
    child
}

#[test]
fn a_looping_runner_that_finds_a_held_head_landed_on_its_chain_alone_never_answers_it_merged() {
    let host = host_under(
        &Check {
            config: "[runner]\nlease_s = 60\nheartbeat_s = 1\n",
            policy: None,
            seed: &[("src/a.rs", "fn a() {}\n")],
            hold_first: true,
            fails: "false",
        },
        Trust::Unsigned,
    );
    let head = host.queue("head", "src");
    let a = host.executor("exec-a", PRODUCER);
    let b = host.executor("exec-b", OTHER_PRODUCER);
    host.test_box
        .git_ok(&b, &["config", "bay.runnerName", "exec-b"]);
    // GIVEN runner A holds the head in its check, and stops once it made the landing's records
    let mut holder = host
        .test_box
        .bay_process(&a)
        .args(["--json", "runner", "run", "--once"])
        .env("BAY_TEST_KILL_AT", "landing-marked")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    host.wait_held();
    // AND looping runner B's visit to the head began on reads that find A holding it
    let dir = host.test_box.path().to_path_buf();
    let (go, journal) = (dir.join("b-go"), dir.join("b-journal"));
    let mut runner = held_looping_runner(&host.test_box, &b, &go, &journal);
    host.go();
    assert_eq!(holder.wait().unwrap().code(), Some(137));
    assert!(!on_main(&host.test_box, &host.remote, &head));
    // AND the remote chain says the head landed, as a client before G875 could leave it
    phantom_landing(&host.test_box, &host.remote, &head);

    // WHEN B's visit reads the chain again for A's lease
    std::fs::write(&go, "go").unwrap();
    let began = Instant::now();
    let answer = loop {
        let text = std::fs::read_to_string(&journal).unwrap_or_default();
        if let Some(line) = text
            .lines()
            .find(|line| line.contains(&format!("{head}.1 [")))
        {
            break line.to_owned();
        }
        assert!(
            began.elapsed() < Duration::from_secs(120),
            "runner B never answered for the head: {text}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = runner.kill();
    let _ = runner.wait();
    // THEN B never answers it merged, and main still lacks it
    assert!(!answer.contains("] merged"), "runner B answered: {answer}");
    assert!(!on_main(&host.test_box, &host.remote, &head));
}

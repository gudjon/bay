//! G867 (a fleet finding, 2026-11-23: a runner took a code change from its own place 18 s after
//! another runner claimed a notes head, before the head's lease named its batch; that batch had
//! taken the code change in, checked for about 1,280 s, and was cut when the change landed, about
//! 21 minutes more for the head). A change passes a head another runner holds only once the
//! holder's lease names what its check covers. And a batch passes over a place another runner
//! holds, and each member whose check reads what that place changes: neither stays in the batch
//! to its landing.
//!
//! Guards to mutate: in `src/publish.rs`, `batch_behind` passing a held head whose lease names no
//! batch (`Some(Vec::new())`: runner B lands the notes change before the holder names its batch),
//! and the `pass_over_held` call in `batch_plan` (runner A's batch takes the held place in again,
//! G852, and is cut); in `src/executor.rs` `Parked::note`, the heartbeat stand of a
//! `batch_unnamed` wait (keep it `STAND_KEPT`: runner B visits the notes change again only once
//! the line moves).

#![cfg(unix)]

use crate::support;

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use support::queued::{Check, Held, OTHER_PRODUCER, Trust, host_under};
use support::{TestBox, check_files, passing_result, result_json, wait_lease_names};

/// An executor clone of `remote` named `name`.
fn executor(test_box: &TestBox, remote: &Path, name: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    clone
}

/// Starts a change on `paths`, writes `file`, and submits it for a runner.
fn submit(test_box: &TestBox, repo: &Path, intent: &str, paths: &str, file: &str) -> String {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", paths]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::create_dir_all(view.join(file).parent().unwrap()).unwrap();
    std::fs::write(view.join(file), "new\n").unwrap();
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    started["change"].as_str().unwrap().to_owned()
}

/// A looping runner in `clone`, its journal (stderr) gathered line by line.
fn looping(test_box: &TestBox, clone: &Path) -> (std::process::Child, Arc<Mutex<Vec<String>>>) {
    let mut child = test_box
        .bay_process(clone)
        .args(["--json", "runner", "run", "--interval-s", "1"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
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

/// How many journal lines so far name operation `op_id`.
fn visits(journal: &Mutex<Vec<String>>, op_id: &str) -> usize {
    journal
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains(op_id))
        .count()
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

/// Waits up to a minute for `path` to exist.
fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_change_passes_a_held_head_only_once_its_lease_names_its_batch() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let base = check_files(
        &format!("[ \"$1\" = code ] && {}", test_box.relay("sleep 25")),
        &passing_result(1),
        1,
        120,
    );
    let script = base
        .iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1
        .clone();
    let check = |id: &str, globs: &str| {
        format!(
            "[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nwhen_paths = [{globs}]\nfootprint = \"observed\"\nexpected_minimum = 1\ntimeout_s = 120\n"
        )
    };
    let policy = format!(
        "revision = 1\n\n{}\n{}",
        check("code", "\"src/**\""),
        check("notes", "\"notes/**\"")
    );
    let files = vec![
        ("scripts/bay-check".to_owned(), script),
        (".bay/policy.toml".to_owned(), policy),
        (
            ".bay/config.toml".to_owned(),
            "[policy]\nevidence_reuse = \"exact-inputs\"\n\n[runner]\nahead = 4\nlease_s = 120\nheartbeat_s = 5\n"
                .to_owned(),
        ),
    ];
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("notes/old.md", "old\n")],
        &files,
    );

    // GIVEN a code change at the head, and a notes change behind it no check of the head reads
    let code = submit(&test_box, &repo, "CODE", "src/", "src/b.rs");
    let notes = submit(&test_box, &repo, "NOTES", "notes/", "notes/new.md");
    let handle = bay::repo::Repo::open(&repo).unwrap();
    bay::queue::join(&handle, None, &format!("{code}.1"), &code).unwrap();
    bay::queue::join(&handle, None, &format!("{notes}.1"), &notes).unwrap();
    // AND runner A claimed the head and has not yet planned what its check covers
    let release = test_box.path().join("release");
    let holder = executor(&test_box, &remote, "exec-a");
    let mut held = test_box
        .bay_process(&holder)
        .env("BAY_TEST_PAUSE_AT", "after-claim")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["runner", "run", "--once"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(
        &test_box.path().join("release.paused"),
        "exec-a never claimed the code change",
    );

    // WHEN another runner, looping, visits the notes change meanwhile, and again
    let other = executor(&test_box, &remote, "exec-b");
    let (mut runner_b, journal) = looping(&test_box, &other);
    let notes_op = format!("{notes}.1");
    let journal_now = || journal.lock().unwrap().join("\n");
    assert!(
        until(Duration::from_secs(60), || visits(&journal, &notes_op) >= 2),
        "the notes change was not visited again: {}",
        journal_now()
    );

    // THEN the notes change waits: the head's batch, not yet named, could hold it
    assert!(
        test_box.trunk_change_ids(&remote).is_empty(),
        "{}",
        journal_now()
    );

    // WHEN runner A goes on, and its lease names the head alone
    std::fs::write(&release, "").unwrap();
    wait_lease_names(&remote, &code, &[format!("{code}.1")]);

    // THEN the notes change lands from its own place while the head's check runs
    assert!(
        until(Duration::from_secs(60), || {
            test_box.trunk_change_ids(&remote) == [notes.clone()]
        }),
        "{}",
        journal_now()
    );
    assert!(
        held.try_wait().unwrap().is_none(),
        "the head's check had ended: {}",
        journal_now()
    );
    // AND the code change lands after it
    assert!(held.wait().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(120);
    while test_box.trunk_change_ids(&remote).len() < 2 {
        assert!(Instant::now() < deadline, "the code change never landed");
        let _ = test_box
            .bay(&holder)
            .args(["runner", "run", "--once"])
            .output();
    }
    runner_b.kill().unwrap();
    runner_b.wait().unwrap();
    assert_eq!(test_box.trunk_change_ids(&remote), vec![code, notes]);
}

/// The policy: check `code` on `src/**`, and check `notes` on `notes/**`, each on what it
/// observed.
fn policy() -> String {
    let check = |id: &str, glob: &str| {
        format!(
            "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"{glob}\"]\nfootprint = \"observed\"\n"
        )
    };
    format!(
        "revision = 1\n\n[policy]\nreuse_unsigned = true\n{}{}",
        check("code", "src/**"),
        check("notes", "notes/**")
    )
}

const CONFIG: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\nbatch = { min = 2, max = 4 }\n";

/// Starts change `intent` on `<dir>/`, adds `<dir>/<intent>.rs` in its view, submits it for a
/// runner and puts it in the queue behind every place there. Returns its id and its view.
fn submit_viewed(held: &Held, intent: &str, dir: &str) -> (String, PathBuf) {
    let started = held.test_box.bay_ok(
        &held.repo,
        &["start", intent, "--paths", &format!("{dir}/")],
    );
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::create_dir_all(view.join(dir)).unwrap();
    std::fs::write(
        view.join(format!("{dir}/{intent}.rs")),
        format!("fn {intent}() {{}}\n"),
    )
    .unwrap();
    let (code, queued) = result_json(
        held.test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    bay::queue::join(&opened, None, &format!("{id}.1"), &id).unwrap();
    (id, view)
}

#[test]
fn a_batch_passes_over_a_place_another_runner_holds_and_what_reads_it() {
    let policy = policy();
    let held = host_under(
        &Check {
            config: CONFIG,
            policy: Some(&policy),
            seed: &[("src/a.rs", "fn a() {}\n"), ("notes/a.rs", "fn a() {}\n")],
            hold_first: true,
            fails: "false",
        },
        Trust::Unsigned,
    );
    // GIVEN a line of a notes head, a code change, a change to notes and code, and a notes change
    let front = held.queue("front", "notes");
    let (taken, view) = submit_viewed(&held, "taken", "src");
    let reader = held.queue_with(
        "reader",
        &["notes", "src"],
        &[
            ("notes/reader.rs", "fn r() {}\n"),
            ("src/reader.rs", "fn r() {}\n"),
        ],
    );
    let member = held.queue("member", "notes");
    // AND the code change's own merge runs it from its own place past the head (G703), and
    // holds it in its check
    let own = held
        .test_box
        .bay_process(&view)
        .args(["--json", "merge", "--wait"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    held.wait_held();

    // WHEN a runner takes the head
    let a = held.executor("exec-a", OTHER_PRODUCER);
    held.test_box
        .git_ok(&a, &["config", "bay.runnerName", "exec-a"]);
    let out = held
        .test_box
        .bay_process(&a)
        .args(["runner", "run", "--once"])
        .output()
        .unwrap();
    let journal = String::from_utf8_lossy(&out.stderr);

    // THEN its batch passes over the held place, and the change whose check reads it
    assert!(
        journal.contains("batch of 2 planned"),
        "no batch of the head and the notes change: {journal}"
    );
    assert!(
        journal.contains(&format!("passed over, another runner holds it: {taken}")),
        "{journal}"
    );
    assert!(
        journal.contains(&format!(
            "passed over, its check reads what a held place changes: {reader}"
        )),
        "{journal}"
    );
    assert!(!journal.contains("could not join"), "{journal}");
    // AND the batch lands
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    for id in [&front, &member] {
        assert!(
            on_main.contains(id),
            "{id} did not land: {on_main:?}\n{journal}"
        );
    }

    // AND once the code change's check ends, every change lands
    held.go();
    let own = own.wait_with_output().unwrap();
    assert!(
        held.test_box
            .trunk_change_ids(&held.remote)
            .contains(&taken),
        "{}",
        String::from_utf8_lossy(&own.stdout)
    );
    let deadline = Instant::now() + Duration::from_secs(120);
    while held.test_box.trunk_change_ids(&held.remote).len() < 4 {
        assert!(Instant::now() < deadline, "not every change landed");
        let _ = held
            .test_box
            .bay_process(&a)
            .args(["runner", "run", "--once"])
            .output();
    }
}

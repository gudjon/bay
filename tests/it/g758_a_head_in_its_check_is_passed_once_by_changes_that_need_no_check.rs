//! G758 (a fleet finding, 2026-11-16: 73 batch members were given back in seven hours with
//! "main moved under the batch": changes that need no check landed past heads whose batches
//! were in their checks, and G552 let them, since it read only the head's yields). A change
//! that needs no check passes a head in its check once; the next waits its turn, so one move
//! costs the head one check, never one per change behind it.
//!
//! Guard to mutate: in `src/publish.rs` `main_moved_under`, drop the `passed` term, and all
//! three pass the head in its check.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use support::{TestBox, check_files, passing_result, result_json};

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

#[test]
fn a_head_in_its_check_is_passed_once_by_changes_that_need_no_check() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let base = check_files(
        &format!("[ \"$1\" = code ] && {}", test_box.relay("sleep 90")),
        &passing_result(1),
        1,
        180,
    );
    let script = base
        .iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1
        .clone();
    let check = |id: &str, globs: &str| {
        format!(
            "[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nwhen_paths = [{globs}]\nexpected_minimum = 1\ntimeout_s = 180\n"
        )
    };
    // Only code is checked: a notes change needs no check (G552). The code check names no
    // footprint, so it reads as reading everything and no change passes by G703's rule.
    let policy = format!("revision = 1\n\n{}", check("code", "\"src/**\""));
    let files = vec![
        ("scripts/bay-check".to_owned(), script),
        (".bay/policy.toml".to_owned(), policy),
        (
            ".bay/config.toml".to_owned(),
            "[runner]\nahead = 8\nlease_s = 240\nheartbeat_s = 5\n".to_owned(),
        ),
    ];
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("notes/old.md", "old\n")],
        &files,
    );

    // GIVEN a code change in its check at the head, and three notes changes behind it
    let code = submit(&test_box, &repo, "CODE", "src/", "src/b.rs");
    let notes: Vec<String> = (1..=3)
        .map(|n| {
            submit(
                &test_box,
                &repo,
                &format!("NOTES-{n}"),
                "notes/",
                &format!("notes/new-{n}.md"),
            )
        })
        .collect();
    let handle = bay::repo::Repo::open(&repo).unwrap();
    bay::queue::join(&handle, None, &format!("{code}.1"), &code).unwrap();
    for id in &notes {
        bay::queue::join(&handle, None, &format!("{id}.1"), id).unwrap();
    }
    let holder = executor(&test_box, &remote, "exec-a");
    let mut held = test_box
        .bay_process(&holder)
        .args(["runner", "run", "--once"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let chain = format!("refs/bay/changes/{code}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !test_box
        .git_bare(&remote, &["log", "--format=%s", &chain])
        .contains("by exec-a")
    {
        assert!(
            Instant::now() < deadline,
            "exec-a never claimed the code change"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // WHEN another runner passes while the head's check runs
    let other = executor(&test_box, &remote, "exec-b");
    let (code_b, ran) = result_json(test_box.bay(&other).args(["runner", "run", "--once"]));
    assert_eq!(code_b, 0, "{ran}");
    let still = held.try_wait().unwrap().is_none();
    let _ = held.kill();
    let _ = held.wait();

    // THEN one notes change passed the head, and the others wait their turn
    assert!(still, "the head's check had ended: {ran}");
    let on_main = test_box.trunk_change_ids(&remote);
    assert_eq!(on_main, vec![notes[0].clone()], "{ran}");
}

//! G703 (owner ruling 2026-11-12; a fleet finding the same day: changes to notes waited 213 to
//! 395 s behind a code change's check that never read them). A change whose files lie outside
//! what the checks of every live change ahead of it read runs from its own place: it is checked
//! and merges while the head's check still runs. The head then lands on the new main on its
//! record, carried because the change ahead of it touched nothing it read.
//!
//! Guard to mutate: in `src/publish.rs` `queue_gate`, the `passes_unread` term (the change waits
//! behind the head until its check ends).

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
fn an_unrelated_change_merges_past_a_head_in_its_check() {
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

    // GIVEN a code change whose check runs for a while at the head, and a notes change behind it
    let code = submit(&test_box, &repo, "CODE", "src/", "src/b.rs");
    let notes = submit(&test_box, &repo, "NOTES", "notes/", "notes/new.md");
    // Both hold places in the line, the code change at its head, as in a busy line.
    let handle = bay::repo::Repo::open(&repo).unwrap();
    bay::queue::join(&handle, None, &format!("{code}.1"), &code).unwrap();
    bay::queue::join(&handle, None, &format!("{notes}.1"), &notes).unwrap();
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

    // THEN the notes change is on main already, ahead of the code change
    let on_main = test_box.trunk_change_ids(&remote);
    assert_eq!(on_main, vec![notes.clone()], "{ran}");
    assert!(
        held.try_wait().unwrap().is_none(),
        "the head's check had ended: {ran}"
    );

    // AND the code change lands after it, on main with the notes change
    assert!(held.wait().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(120);
    while test_box.trunk_change_ids(&remote).len() < 2 {
        assert!(Instant::now() < deadline, "the code change never landed");
        let _ = test_box
            .bay(&holder)
            .args(["runner", "run", "--once"])
            .output();
    }
    assert_eq!(test_box.trunk_change_ids(&remote), vec![code, notes]);
}

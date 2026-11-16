//! G753 (a fleet finding, 2026-11-16: four changes merged past one head in 23 minutes while it
//! checked and then waited for another box's runner; the cap of G703 counted only the head's
//! yields, and it had yielded once, so its batch of twenty was checked again and again). The
//! cap counts every change that landed past an entry while it was live: a head still in its
//! check lets three unrelated changes pass it, and the fourth waits its turn.
//!
//! Guard to mutate: in `src/publish.rs` `moves_under`, count the yields only (drop `passed`),
//! and all four pass the head in its check.

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
fn a_head_in_its_check_is_passed_three_times_and_no_more() {
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
            "[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nwhen_paths = [{globs}]\nfootprint = \"observed\"\nexpected_minimum = 1\ntimeout_s = 180\n"
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
            "[policy]\nevidence_reuse = \"exact-inputs\"\n\n[runner]\nahead = 8\nlease_s = 240\nheartbeat_s = 5\n"
                .to_owned(),
        ),
    ];
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("notes/old.md", "old\n")],
        &files,
    );

    // GIVEN a code change in its check at the head, and four notes changes behind it
    let code = submit(&test_box, &repo, "CODE", "src/", "src/b.rs");
    let notes: Vec<String> = (1..=4)
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

    // THEN three notes changes passed the head, and the fourth waits its turn
    assert!(still, "the head's check had ended: {ran}");
    let on_main = test_box.trunk_change_ids(&remote);
    assert_eq!(on_main.len(), 3, "{on_main:?} {ran}");
    assert!(!on_main.contains(&code), "{on_main:?}");
    assert!(
        !on_main.contains(&notes[3]),
        "the fourth passed too: {on_main:?}"
    );
}

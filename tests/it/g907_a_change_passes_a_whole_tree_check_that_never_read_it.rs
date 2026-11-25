//! G907 (a fleet measurement, 2026-11-25: on a repository whose whole-tree check lists `**`, no
//! change ever passed a head in its check, since that glob's literal prefix meets every path). A
//! check with `footprint = "observed"` reads, for a pass, what its records on main near the
//! trunk read and listed, exactly: a change to a file it never read merges past a head in that
//! check, and the head lands after it on its record, carried.
//!
//! Guard to mutate: in `src/publish.rs` `predicted_reads`, the `observed_near` term (the
//! check reads its `when_paths`, `**`, and the change waits behind the head).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

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

/// Runs `runner` until main holds `count` changes.
fn land(test_box: &TestBox, runner: &Path, remote: &Path, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while test_box.trunk_change_ids(remote).len() < count {
        assert!(Instant::now() < deadline, "main never held {count} changes");
        let _ = test_box
            .bay(runner)
            .args(["runner", "run", "--once"])
            .output();
    }
}

#[test]
fn a_change_passes_a_whole_tree_check_that_never_read_it() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let base = check_files(
        &format!("[ -f src/b.rs ] && {}", test_box.relay("sleep 25")),
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
    let policy = "revision = 1\n\n[[check]]\nid = \"tree\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check tree\"\nadapter = [\"scripts/bay-check\"]\nwhen_paths = [\"**\"]\nfootprint = \"observed\"\nexpected_minimum = 1\ntimeout_s = 120\n";
    let files = vec![
        ("scripts/bay-check".to_owned(), script),
        (".bay/policy.toml".to_owned(), policy.to_owned()),
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

    // GIVEN a landing whose check observed what it read, on main
    let warm = submit(&test_box, &repo, "WARM", "src/", "src/a.rs");
    let holder = executor(&test_box, &remote, "exec-a");
    land(&test_box, &holder, &remote, 1);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![warm.clone()]);

    // AND a code change whose check runs for a while at the head, and a notes edit behind it
    let code = submit(&test_box, &repo, "CODE", "src/", "src/b.rs");
    let notes = submit(&test_box, &repo, "NOTES", "notes/", "notes/old.md");
    let handle = bay::repo::Repo::open(&repo).unwrap();
    bay::queue::join(&handle, None, &format!("{code}.1"), &code).unwrap();
    bay::queue::join(&handle, None, &format!("{notes}.1"), &notes).unwrap();
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
    wait_lease_names(&remote, &code, &[format!("{code}.1")]);

    // WHEN another runner passes while the head's check runs
    let other = executor(&test_box, &remote, "exec-b");
    let (code_b, ran) = result_json(test_box.bay(&other).args(["runner", "run", "--once"]));
    assert_eq!(code_b, 0, "{ran}");

    // THEN the notes edit is on main already, ahead of the code change
    let on_main = test_box.trunk_change_ids(&remote);
    assert_eq!(on_main, vec![notes.clone(), warm.clone()], "{ran}");
    assert!(
        held.try_wait().unwrap().is_none(),
        "the head's check had ended: {ran}"
    );

    // AND the code change lands after it
    assert!(held.wait().unwrap().success());
    land(&test_box, &holder, &remote, 3);
    assert_eq!(test_box.trunk_change_ids(&remote), vec![code, notes, warm]);
}

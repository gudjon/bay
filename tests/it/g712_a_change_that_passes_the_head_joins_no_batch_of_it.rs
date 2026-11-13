//! G712 (a fleet finding, 2026-11-13: a code change's batch took in two notes changes; each
//! merged from its own place by G703 while the batch was checked, so the batch was cut and
//! checked again twice, 189 s and 370 s lost, and changes behind it waited 16 to 35 minutes). A
//! change that runs from its own place past the head joins no batch of that head: the head plans
//! its batch from the changes that wait for it, and the change that passes lands alone.
//!
//! Guard to mutate: in `src/publish.rs` `pass_over_passing`, the line that marks such a change not
//! batchable (the notes change joins the batch again).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

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
fn a_change_that_passes_the_head_joins_no_batch_of_it() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let base = check_files("true", &passing_result(1), 1, 120);
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
        "revision = 1\n\n[policy]\nreuse_unsigned = true\n\n{}\n{}",
        check("code", "\"src/**\""),
        check("notes", "\"notes/**\"")
    );
    let files = vec![
        ("scripts/bay-check".to_owned(), script),
        (".bay/policy.toml".to_owned(), policy),
        (
            ".bay/config.toml".to_owned(),
            "[policy]\nevidence_reuse = \"exact-inputs\"\n\n[runner]\nbatch = { max = 8 }\n"
                .to_owned(),
        ),
    ];
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("notes/old.md", "old\n")],
        &files,
    );

    // GIVEN a line of a code change, a notes change no check of it reads, and a second code
    // change
    let first = submit(&test_box, &repo, "CODE ONE", "src/", "src/b.rs");
    let notes = submit(&test_box, &repo, "NOTES", "notes/", "notes/new.md");
    let second = submit(&test_box, &repo, "CODE TWO", "src/", "src/c.rs");
    let handle = bay::repo::Repo::open(&repo).unwrap();
    for id in [&first, &notes, &second] {
        bay::queue::join(&handle, None, &format!("{id}.1"), id).unwrap();
    }

    // WHEN the runner takes the head
    let runner = executor(&test_box, &remote, "exec-a");
    let out = test_box
        .bay(&runner)
        .args(["runner", "run", "--once"])
        .output()
        .unwrap();
    let journal = String::from_utf8_lossy(&out.stderr);

    // THEN it batches the two code changes, and the notes change is no member of it
    assert!(
        journal.contains("batch of 2 planned"),
        "no batch of the two code changes: {journal}"
    );
    assert!(!journal.contains("batch of 3"), "{journal}");
    assert!(!journal.contains("could not join"), "{journal}");
    let on_main = test_box.trunk_change_ids(&remote);
    assert!(
        on_main.contains(&first) && on_main.contains(&second),
        "{on_main:?}\n{journal}"
    );
}

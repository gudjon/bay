//! G852 (a fleet finding, 2026-11-22: a code change was passed over as one that passes a head on
//! notes (G712), and the head's batch then took in three code changes from behind it; the
//! batch's code check read the passer, so it could not pass that batch (G846). It waited the
//! whole batch out and was checked alone after it: 63 minutes in the line). A change passed over
//! as one that passes the head joins the head's batch again when the batch's check reads it, and
//! lands in the batch's one check.
//!
//! Guard to mutate: in `src/publish.rs` `batch_plan`, the loop that plans the batch again when
//! `readmit_read` readmits a passer (the batch is of two, and the passer is left out).

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
fn a_change_that_passes_the_head_joins_its_batch_when_the_batch_reads_it() {
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

    // GIVEN a line of a notes change, a code change no check of the notes change reads, and a
    // second code change
    let notes = submit(&test_box, &repo, "NOTES", "notes/", "notes/new.md");
    let first = submit(&test_box, &repo, "CODE ONE", "src/", "src/b.rs");
    let second = submit(&test_box, &repo, "CODE TWO", "src/", "src/c.rs");
    let handle = bay::repo::Repo::open(&repo).unwrap();
    for id in [&notes, &first, &second] {
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

    // THEN the first code change, which passes the notes head, is in the batch all the same:
    // the batch's code check reads it, so it could not pass that batch
    assert!(
        journal.contains("batch of 3 planned"),
        "no batch of all three: {journal}"
    );
    assert!(!journal.contains("lands from its own place"), "{journal}");
    assert!(!journal.contains("could not join"), "{journal}");
    // AND the one check lands all three
    let on_main = test_box.trunk_change_ids(&remote);
    for id in [&notes, &first, &second] {
        assert!(
            on_main.contains(id),
            "{id} did not land: {on_main:?}\n{journal}"
        );
    }
}

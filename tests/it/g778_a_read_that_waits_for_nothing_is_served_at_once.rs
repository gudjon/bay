//! G778 (owner, 2026-11-17: a panel that shows bay was still very long to load; a seat's
//! `bay status --deadline 0` took 8-11 s, and a read of one change 4.5 s alone and 77 s beside the
//! panel's other reads, each built again from the store: no saved answer served a linked worktree,
//! a named change, or a store whose refs had moved, and on a busy store refs move every second).
//! A read at `--deadline 0` answers with what this machine last saved for that read (the actor,
//! the checkout, the change), whatever moved since, naming its age; it runs no Git of its own,
//! however many changes the store holds, and has the answer read again in the background so the
//! next read is new. Only a read with nothing saved builds its answer, and saves it.
//!
//! Guard to mutate: in `src/status.rs` `from_store`, answer `None`, and the second read builds
//! again with Git.

use crate::support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

const ACTOR: &str = "seat-one";

/// A read at `--deadline 0` of `change` (or the listing) in `dir`: its answer and the Git commands
/// its own process ran.
fn read_now(test_box: &TestBox, dir: &Path, change: Option<&str>, tag: &str) -> (Value, usize) {
    let trace = test_box.path().join(format!("trace-{tag}.jsonl"));
    let _ = std::fs::remove_file(&trace);
    let mut args = vec!["status"];
    args.extend(change);
    args.extend(["--deadline", "0"]);
    let (code, answer) = result_json(
        test_box
            .bay(dir)
            .args(&args)
            .env("BAY_ACTOR", ACTOR)
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{answer}");
    let text = std::fs::read_to_string(&trace).unwrap_or_default();
    let lines: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    // The read's own process: a read again it starts in the background traces under its own.
    let pid = answer_pid(&lines);
    let own: Vec<&Value> = lines
        .iter()
        .filter(|line| pid.is_none() || line["pid"] == *pid.as_ref().unwrap_or(&Value::Null))
        .collect();
    (answer, own.len())
}

/// The process that ran the first traced Git command.
fn answer_pid(lines: &[Value]) -> Option<Value> {
    lines.first().map(|line| line["pid"].clone())
}

/// A store with `changes` changes of the seat's, and the view of the first.
fn store(test_box: &TestBox, changes: usize) -> (PathBuf, String, PathBuf) {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let mut first = None;
    for n in 0..changes {
        let (code, started) = result_json(
            test_box
                .bay(&repo)
                .args(["start", &format!("CARD-{n}"), "--paths", "src/"])
                .env("BAY_ACTOR", ACTOR),
        );
        assert_eq!(code, 0, "{started}");
        if first.is_none() {
            first = Some((
                started["change"].as_str().unwrap().to_owned(),
                PathBuf::from(started["detail"]["view"].as_str().unwrap()),
            ));
        }
    }
    let (id, view) = first.unwrap();
    (repo, id, view)
}

/// The Git commands of a second `--deadline 0` read, of one change in its view and of the
/// listing, in a store of `changes` changes.
fn second_reads(changes: usize) -> (usize, usize) {
    let test_box = TestBox::new();
    let (repo, id, view) = store(&test_box, changes);
    let _ = read_now(&test_box, &view, Some(&id), "one-first");
    let _ = read_now(&test_box, &repo, None, "all-first");
    let (one, one_calls) = read_now(&test_box, &view, Some(&id), "one-second");
    let (all, all_calls) = read_now(&test_box, &repo, None, "all-second");
    assert_eq!(one["detail"]["from_store"], true, "{one}");
    assert_eq!(all["detail"]["from_store"], true, "{all}");
    assert_eq!(one["change"], id.as_str(), "{one}");
    (one_calls, all_calls)
}

#[test]
fn a_second_read_runs_no_git_however_many_changes() {
    // GIVEN stores of 3 and of 12 changes, each read once at `--deadline 0`
    let small = second_reads(3);
    let large = second_reads(12);

    // THEN the next reads run no Git of their own in either
    assert_eq!(small, (0, 0), "a second read ran Git: {small:?}");
    assert_eq!(large, (0, 0), "a second read ran Git: {large:?}");
}

#[test]
fn a_read_after_a_ref_moved_is_served_with_its_age_and_read_again_behind_it() {
    let test_box = TestBox::new();
    let (repo, id, view) = store(&test_box, 2);
    let _ = read_now(&test_box, &view, Some(&id), "first");
    let saved = saved_at(&repo);
    // GIVEN a ref of the store moved after the answer was saved
    test_box.git_ok(&repo, &["update-ref", "refs/bay/moved-after-save", "HEAD"]);
    std::thread::sleep(Duration::from_millis(1100));

    // WHEN the change is read again at once, with any saved answer old enough to read again
    let (code, answer) = result_json(
        test_box
            .bay(&view)
            .args(["status", &id, "--deadline", "0"])
            .env("BAY_ACTOR", ACTOR)
            .env("BAY_TEST_REFRESH_AFTER_S", "0"),
    );
    assert_eq!(code, 0, "{answer}");

    // THEN it is the saved answer, naming its age
    assert_eq!(answer["detail"]["from_store"], true, "{answer}");
    assert!(answer["detail"]["age_s"].as_u64().is_some(), "{answer}");
    // AND a read again behind it saves a newer answer
    let until = Instant::now() + Duration::from_secs(60);
    while saved_at(&repo) == saved && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(250));
    }
    assert_ne!(saved_at(&repo), saved, "no read again saved a newer answer");
}

/// The times the saved answers of `repo`'s store were saved at.
fn saved_at(repo: &Path) -> Vec<u64> {
    let dir = repo.join(".git").join("bay").join("status-answers");
    let mut times: Vec<u64> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
                .filter_map(|saved| saved["saved"].as_u64())
                .collect()
        })
        .unwrap_or_default();
    times.sort_unstable();
    times
}

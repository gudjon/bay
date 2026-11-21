//! G835 (a fleet finding, 2026-11-21: a store read of the line took about 1.1 s on a store of
//! 10,331 changes, where one of the release before G830 took 70 ms, as it read every change's
//! record to pass over the merge requests already answered). A read keeps each request it found
//! answered with the chain head that answered it, and a later read passes over that pair without
//! reading the record again.
//!
//! The case removes the landed change's record from the reading store after the first read: a
//! second read that read it again would find none and name the change unread. The answers kept
//! for fixed Git queries are off (`BAY_TEST_FACTS=off`), so only the kept pair can spare the read.
//!
//! G836 (a fleet finding, 2026-11-21: after G835, a store read still read 273 records twice, each
//! of an abandoned change whose operations the retention cut removed). A request the line passes
//! over by the G597 rule is kept answered too.
//!
//! Guards to mutate: in `src/line.rs` `unanswered`, read no kept pair (`kept` empty), or keep
//! only a pair whose record says its operation waits for nothing (`waits(record, request) ==
//! Some(false)` in place of `passed_over`). The second read then reads the record again, finds
//! none, and names the change unread.

#![cfg(unix)]

use crate::support;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use support::PRODUCER;
use support::TestBox;
use support::queued::{Check, host};

/// Submits a change of `file` from `repo`, and returns its id.
fn submit(test_box: &TestBox, repo: &Path, file: &str) -> String {
    let started = test_box.bay_ok(repo, &["start", file, "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(file), "fn x() { 1; }\n").unwrap();
    let (code, submitted) = support::result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    id
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Removes object `id` from the store of `repo`: every pack is unpacked first, so the object
/// is a file of its own. The packs and their indexes go before any is unpacked, or Git would
/// take their objects as held and write none of them.
fn remove_object(repo: &Path, id: &str) {
    let objects = repo.join(".git/objects");
    let packs = objects.join("pack");
    let mut held = Vec::new();
    for entry in std::fs::read_dir(&packs).unwrap().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "pack") {
            held.push(std::fs::read(&path).unwrap());
        }
    }
    std::fs::remove_dir_all(&packs).unwrap();
    std::fs::create_dir(&packs).unwrap();
    for bytes in held {
        let mut unpack = Command::new("git")
            .current_dir(repo)
            .args(["unpack-objects", "-q"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        unpack.stdin.take().unwrap().write_all(&bytes).unwrap();
        assert!(unpack.wait().unwrap().success());
    }
    std::fs::remove_file(objects.join(&id[..2]).join(&id[2..])).unwrap();
}

/// `bay queue --deadline 0` in `repo`, with no answers of fixed Git queries kept: the line.
fn read_line(test_box: &TestBox, repo: &Path) -> Value {
    let (code, line) = support::result_json(
        test_box
            .bay(repo)
            .args(["queue", "--deadline", "0"])
            .env("BAY_TEST_FACTS", "off"),
    );
    assert_eq!(code, 0, "{line}");
    support::sweeps_ended(repo);
    line
}

fn arriving(line: &Value) -> Vec<&str> {
    line["detail"]["arriving"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["change"].as_str().unwrap())
        .collect()
}

/// Whether `repo` holds object `name`.
fn present(repo: &Path, name: &str) -> bool {
    Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "-e", name])
        .status()
        .unwrap()
        .success()
}

/// Removes the record of `change` from the store of `repo`, and asserts that its chain head and
/// its request stay.
fn remove_record(repo: &Path, change: &str) {
    let record = git(
        repo,
        &[
            "rev-parse",
            &format!("refs/bay/changes/{change}:change.json"),
        ],
    );
    remove_object(repo, &record);
    assert!(
        present(repo, &format!("refs/bay/changes/{change}")),
        "the chain head left too"
    );
    assert!(
        present(repo, &format!("refs/bay/requests/{change}")),
        "the request left too"
    );
    assert!(!present(repo, &record), "the record is still held");
}

#[test]
fn a_store_read_passes_over_an_answered_request_without_reading_its_record_again() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")],
        &support::check_files("true", &support::passing_result(1), 1, 60),
    );
    let remote = test_box.path().join("repo-remote.git");

    // GIVEN one change that a runner landed
    let landed = submit(&test_box, &repo, "src/a.rs");
    let executor = test_box.clone_of(&remote, "executor");
    for _ in 0..4 {
        if test_box.trunk_change_ids(&remote).contains(&landed) {
            break;
        }
        let _ = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    }
    assert_eq!(test_box.trunk_change_ids(&remote), vec![landed.clone()]);

    // AND one change submitted after it, which no runner has placed
    let waiting = submit(&test_box, &repo, "src/b.rs");

    // AND a machine whose status swept the remote, and whose store read of the line lists only
    // the change that waits
    let reader = test_box.clone_of(&remote, "reader");
    let _ = test_box.bay_ok(&reader, &["status"]);
    support::sweeps_ended(&reader);
    let first = read_line(&test_box, &reader);
    assert_eq!(first["detail"]["from_store"], true, "{first}");
    assert_eq!(arriving(&first), vec![waiting.as_str()], "{first}");
    assert_eq!(first["detail"]["unread"], serde_json::json!([]), "{first}");

    // WHEN the landed change's record leaves that store, and it reads the line again
    remove_record(&reader, &landed);
    let second = read_line(&test_box, &reader);

    // THEN the change that waits is listed, and the landed change is neither listed nor unread
    assert_eq!(second["detail"]["from_store"], true, "{second}");
    assert_eq!(arriving(&second), vec![waiting.as_str()], "{second}");
    assert_eq!(
        second["detail"]["unread"],
        serde_json::json!([]),
        "{second}"
    );
}

#[test]
fn a_store_read_passes_over_the_request_of_a_cut_abandoned_change_without_reading_it_again() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n\n[retention]\nwindow_s = 0\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let runner = held.executor("exec-a", PRODUCER);

    // GIVEN a submitted change that was abandoned and cut by retention, its request ref still
    // on the remote
    let started = held
        .test_box
        .bay_ok(&held.repo, &["start", "dropped", "--paths", "src/"]);
    let dropped = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/dropped.rs"), "fn dropped() {}\n").unwrap();
    let submitted = held.test_box.bay_ok(&view, &["merge"]);
    assert_eq!(submitted["decision"], "queued", "{submitted}");
    let abandoned = held.test_box.bay_ok(&view, &["abandon"]);
    assert_eq!(abandoned["decision"], "ok", "{abandoned}");
    let record = |change: &str| -> Value {
        let text = held.test_box.git_bare(
            &held.remote,
            &["show", &format!("refs/bay/changes/{change}:change.json")],
        );
        serde_json::from_str(&text).unwrap()
    };
    for _ in 0..6 {
        if record(&dropped)["retention_cut"] == true {
            break;
        }
        let _ = held.pass(&runner);
    }
    assert_eq!(
        record(&dropped)["retention_cut"],
        true,
        "{}",
        record(&dropped)
    );
    assert_eq!(record(&dropped)["state"], "abandoned");

    // AND a machine whose status swept the remote, and whose store read lists nothing
    let reader = held.test_box.clone_of(&held.remote, "reader");
    let _ = held.test_box.bay_ok(&reader, &["status"]);
    support::sweeps_ended(&reader);
    let first = read_line(&held.test_box, &reader);
    assert_eq!(first["detail"]["from_store"], true, "{first}");
    assert_eq!(arriving(&first), Vec::<&str>::new(), "{first}");
    assert_eq!(first["detail"]["unread"], serde_json::json!([]), "{first}");

    // WHEN the cut change's record leaves that store, and it reads the line again
    remove_record(&reader, &dropped);
    let second = read_line(&held.test_box, &reader);

    // THEN nothing is listed and nothing is unread
    assert_eq!(second["detail"]["from_store"], true, "{second}");
    assert_eq!(arriving(&second), Vec::<&str>::new(), "{second}");
    assert_eq!(
        second["detail"]["unread"],
        serde_json::json!([]),
        "{second}"
    );
}

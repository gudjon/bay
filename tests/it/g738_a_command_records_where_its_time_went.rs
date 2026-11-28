//! G738 (a measurement, 2026-11-15: where agents wait on bay could be read only from their own
//! transcripts, and never said where a command's time went). A command run in a repository
//! appends one line to `bay/spent/<day>.jsonl` in the store's common directory: its command, its
//! decision, its wall time, its time in Git and on the network, and its slowest Git commands. A
//! command run in a view records in the store it shares. G893: the line also says what the
//! command wrote: how it chose its output, and its bytes on stdout and stderr.
//!
//! Guard to mutate: in `src/main.rs`, drop the call of `spent` before the result is rendered, and
//! no line is written.

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, unchecked_files};

/// Every line recorded in the store at `common`.
fn lines(common: &Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(common.join("bay").join("spent")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .flat_map(|entry| {
            std::fs::read_to_string(entry.path())
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect::<Vec<Value>>()
        })
        .collect()
}

#[test]
fn a_status_and_a_start_in_a_view_each_record_one_line_in_the_store() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let common = repo.join(".git");
    let before = lines(&common).len();

    // WHEN a status runs in the repository and a status runs in a view
    test_box.bay_ok(&repo, &["status"]);
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    test_box.bay_ok(&view, &["status"]);

    // THEN each recorded its line in the store's common directory
    let recorded = lines(&common);
    let new: Vec<&Value> = recorded.iter().skip(before).collect();
    let commands: Vec<&str> = new
        .iter()
        .filter_map(|line| line["command"].as_str())
        .collect();
    assert_eq!(commands, ["status", "start", "status"], "{new:?}");
    for line in &new {
        assert_eq!(line["decision"], "ok", "{line}");
        assert!(line["ms"].is_u64() && line["git_ms"].is_u64(), "{line}");
        assert!(
            line["git_calls"].as_u64().is_some_and(|calls| calls > 0),
            "{line}"
        );
        assert!(line["slowest"].is_array(), "{line}");
        assert_eq!(line["retried"], serde_json::json!([]), "{line}");
    }
}

#[test]
fn a_line_says_how_its_command_wrote_and_how_many_bytes() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let common = repo.join(".git");

    // WHEN a status runs through a pipe to a person who reads stderr (G961), then with `--json`
    let piped = test_box
        .bay(&repo)
        .env("BAY_TEST_STDERR_TERMINAL", "1")
        .arg("status")
        .output()
        .unwrap();
    let json = test_box
        .bay(&repo)
        .args(["--json", "status"])
        .output()
        .unwrap();

    // THEN each line names its output and counts what went to each stream
    let recorded = lines(&common);
    let shown: Vec<&Value> = recorded.iter().map(|line| &line["shown"]).collect();
    let [.., piped_line, json_line] = shown.as_slice() else {
        panic!("{recorded:?}");
    };
    assert_eq!(piped_line["as"], "pipe", "{piped_line}");
    assert_eq!(piped_line["stdout"], piped.stdout.len(), "{piped_line}");
    let err = piped_line["stderr"].as_u64().unwrap();
    assert!(
        err > 0 && usize::try_from(err).unwrap() <= piped.stderr.len(),
        "{piped_line}"
    );
    assert_eq!(piped_line["stderr_terminal"], false, "{piped_line}");
    assert_eq!(json_line["as"], "json", "{json_line}");
    assert_eq!(json_line["stdout"], json.stdout.len(), "{json_line}");
    assert_eq!(json_line["stderr"], 0, "{json_line}");
}

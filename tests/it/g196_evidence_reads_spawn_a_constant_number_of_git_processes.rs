//! G196. Evidence reads use a constant number of git processes, whatever the number of
//! producers: one `for-each-ref` lists the producer refs and one `git cat-file --batch` reads
//! every producer's note on a tree, never one `git notes show` per producer. `bay status` of a
//! landed change spawns the same `git` processes with one producer as with five, counted from
//! the trace a test build writes when `BAY_TEST_GIT_TRACE` names a file (G171), and the
//! records it reads are the ones `git notes show` gives, at whatever fanout the notes tree
//! holds them.

// The landing runs a shell check adapter, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bay::evidence::{self, EVIDENCE_PREFIX, EvidenceRecord};
use bay::git::Git;
use serde_json::Value;
use support::{PRODUCER, TestBox, check_files, passing_result, result_json};

/// A box with one change landed under a passing check. Returns the box, the repo, the change
/// id and the landed tree.
fn landed() -> (TestBox, PathBuf, String, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let files = check_files("true", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    // A status fetches the evidence notes, and names the tree the check's record is on.
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let tree = status["detail"]["changes"][0]["evidence"]["tree"]
        .as_str()
        .unwrap_or_else(|| panic!("no evidence tree: {status}"))
        .to_owned();
    (test_box, repo, id, tree)
}

/// The check's record on `tree`, as producer `name` would write it about the same tree.
fn record_of(git: &Git, tree: &str, name: &str) -> EvidenceRecord {
    // The producer that recorded the check: the one evidence notes ref the landing wrote.
    let refs = git
        .run(&[
            "for-each-ref",
            "--format=%(refname)",
            evidence::EVIDENCE_PREFIX,
        ])
        .unwrap();
    let producer = refs
        .lines()
        .find_map(|name| name.strip_prefix(evidence::EVIDENCE_PREFIX))
        .unwrap_or(PRODUCER);
    let mut record = evidence::producer_records(git, producer, tree)
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("the check's record under {producer}: refs {refs:?}"));
    name.clone_into(&mut record.producer.name);
    record.id = format!("{name}/check");
    record
}

/// The `git` spawns of bay's own process during one `bay status <id>`, by subcommand, and the
/// status result.
fn status_spawns(
    test_box: &TestBox,
    repo: &Path,
    id: &str,
    name: &str,
) -> (BTreeMap<String, usize>, Value) {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let (code, result) = result_json(
        test_box
            .bay(repo)
            .args(["status", id])
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{result}");
    let text = std::fs::read_to_string(&trace).expect("the trace was written");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let mut counts = BTreeMap::new();
    for line in lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref())
    {
        *counts
            .entry(line["sub"].as_str().unwrap_or_default().to_owned())
            .or_insert(0) += 1;
    }
    (counts, result)
}

/// Every record on `tree` as Git itself reads it: one `git notes show` per producer ref, in
/// ref order.
fn as_git_shows(test_box: &TestBox, repo: &Path, tree: &str) -> Vec<EvidenceRecord> {
    let refs = test_box.git_ok(
        repo,
        &["for-each-ref", "--format=%(refname)", EVIDENCE_PREFIX],
    );
    let mut all = Vec::new();
    for name in refs.lines() {
        let out = test_box
            .git(repo)
            .args(["notes", &format!("--ref={name}"), "show", tree])
            .output()
            .unwrap();
        if out.status.success() {
            let records: Vec<EvidenceRecord> = serde_json::from_slice(&out.stdout)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            all.extend(records);
        }
    }
    all
}

/// The record ids `bay status` lists on the change's evidence.
fn listed_ids(status: &Value) -> Vec<String> {
    let mut ids: Vec<String> = status["detail"]["changes"][0]["evidence"]["records"]
        .as_array()
        .unwrap_or_else(|| panic!("{status}"))
        .iter()
        .map(|record| record["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

fn sorted_ids(records: &[EvidenceRecord]) -> Vec<String> {
    let mut ids: Vec<String> = records.iter().map(|record| record.id.clone()).collect();
    ids.sort();
    ids
}

#[test]
fn a_status_spawns_as_many_git_processes_with_five_producers_as_with_one() {
    let (test_box, repo, id, tree) = landed();
    let git = Git::new(&repo);
    // A first status writes what later ones read (the coordination cache), so it is not counted.
    status_spawns(&test_box, &repo, &id, "warm");
    let (one, status) = status_spawns(&test_box, &repo, &id, "one");
    assert_eq!(
        sorted_ids(&as_git_shows(&test_box, &repo, &tree)),
        listed_ids(&status),
        "{status}"
    );

    for extra in 2..=5 {
        let name = format!("producer-{extra}");
        evidence::append(&git, &name, &record_of(&git, &tree, &name)).unwrap();
    }
    let shown = as_git_shows(&test_box, &repo, &tree);
    assert_eq!(shown.len(), 5, "{shown:?}");
    let (five, status) = status_spawns(&test_box, &repo, &id, "five");
    assert_eq!(
        one, five,
        "bay status spawned other git processes with five producers than with one"
    );
    assert_eq!(sorted_ids(&shown), listed_ids(&status), "{status}");
    assert_eq!(evidence::records_for(&git, &tree).unwrap(), shown);
}

/// A notes ref of `name` holding `record` on `tree` at `levels` of two-hex-digit fanout.
fn fanned_note(test_box: &TestBox, repo: &Path, tree: &str, name: &str, levels: usize) {
    let mut text =
        serde_json::to_string_pretty(&vec![record_of(&Git::new(repo), tree, name)]).unwrap();
    text.push('\n');
    let mktree = |entry: &str| {
        let out = test_box
            .git(repo)
            .arg("mktree")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().unwrap().write_all(entry.as_bytes())?;
                child.wait_with_output()
            })
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    let blob_path = test_box.path().join(format!("{name}.json"));
    std::fs::write(&blob_path, text).unwrap();
    let blob = test_box.git_ok(repo, &["hash-object", "-w", blob_path.to_str().unwrap()]);
    let mut object = blob;
    let mut kind = "100644 blob";
    let mut path = tree[levels * 2..].to_owned();
    for level in (0..levels).rev() {
        object = mktree(&format!("{kind} {object}\t{path}\n"));
        kind = "040000 tree";
        tree[level * 2..level * 2 + 2].clone_into(&mut path);
    }
    let top = mktree(&format!("{kind} {object}\t{path}\n"));
    let commit = test_box.git_ok(repo, &["commit-tree", "-m", "Notes", &top]);
    test_box.git_ok(repo, &["update-ref", &evidence::notes_ref(name), &commit]);
}

#[test]
fn a_note_is_read_at_whatever_fanout_its_notes_tree_holds_it() {
    let (test_box, repo, _, tree) = landed();
    fanned_note(&test_box, &repo, &tree, "fanned-1", 1);
    fanned_note(&test_box, &repo, &tree, "fanned-2", 2);
    let shown = as_git_shows(&test_box, &repo, &tree);
    assert_eq!(shown.len(), 3, "{shown:?}");
    assert_eq!(
        evidence::records_for(&Git::new(&repo), &tree).unwrap(),
        shown
    );
}

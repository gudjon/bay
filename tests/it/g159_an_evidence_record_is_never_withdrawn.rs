//! G159. An evidence record is never withdrawn.
//!
//! On a bay-hosted trunk with a registry, a producer pushes its own notes ref. A push whose
//! note for a tree drops a record the old note had, or changes one, is refused with
//! `EVIDENCE_WITHDRAWN`, and the host keeps the old note: a producer cannot hide its own
//! `product_failure`. No retention cut of evidence notes exists yet, so every removal is
//! refused (owner ruling Q1: only a registry-signed retention cut may remove one). Adding a
//! record still works. A note is not canonical bytes (it is the JSON list in append order), so a
//! rewrite that keeps every record byte for byte, such as one in another order, is accepted.

// The host hook runs as a shell-installed `pre-receive`, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use bay::evidence::EvidenceRecord;
use support::{PRODUCER, TestBox, TestKeys, check_files, passing_result, result_json};

const NOTES: &str = "refs/notes/bay/evidence/ci-1";

/// A bay host with a registry, an agent clone using the keys, and one landed change. Returns
/// the landed tree and commit too.
fn landed() -> (TestBox, PathBuf, PathBuf, TestKeys, String, String) {
    let test_box = TestBox::new();
    let files = check_files("true", &passing_result(1), 1, 60);
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, out) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{out}");
    test_box.git_ok(&repo, &["fetch", "-q", "origin"]);
    test_box.git_ok(
        &repo,
        &["fetch", "-q", "origin", &format!("+{NOTES}:{NOTES}")],
    );
    let tree = test_box.git_ok(&repo, &["rev-parse", "origin/main^{tree}"]);
    let commit = test_box.git_ok(&repo, &["rev-parse", "origin/main"]);
    (test_box, host, repo, keys, tree, commit)
}

/// A producer-signed record of `verdict` about `tree`, under `id`.
fn record(keys: &TestKeys, tree: &str, commit: &str, id: &str, verdict: &str) -> serde_json::Value {
    let mut parsed: EvidenceRecord = serde_json::from_value(serde_json::json!({
        "schema": "bay.evidence/1", "id": format!("{PRODUCER}/{id}"), "kind": "check",
        "subject": {"tree": tree, "commit": commit, "base_tree": tree},
        "check": {"id": "tests", "environment": "local"}, "policy_revision": 1,
        "manifest_digest": "sha256:x", "producer": {"name": PRODUCER, "version": "1"},
        "verdict": verdict, "at": "2026-10-19T00:00:00Z"}))
    .unwrap();
    parsed.sign(&keys.producer).unwrap();
    serde_json::to_value(parsed).unwrap()
}

fn note(test_box: &TestBox, dir: &Path, tree: &str) -> Vec<serde_json::Value> {
    let text = test_box.git_ok(dir, &["notes", &format!("--ref={NOTES}"), "show", tree]);
    serde_json::from_str(&text).unwrap()
}

/// Writes `records` as the producer's note on `tree`, as a new notes commit on top.
fn write_note(test_box: &TestBox, repo: &Path, tree: &str, records: &[serde_json::Value]) {
    let text = serde_json::to_string_pretty(records).unwrap();
    test_box.git_ok(
        repo,
        &[
            "notes",
            &format!("--ref={NOTES}"),
            "add",
            "-f",
            "-m",
            &text,
            tree,
        ],
    );
}

fn push(test_box: &TestBox, repo: &Path) -> (bool, String) {
    let out = test_box
        .git(repo)
        .args(["push", "origin", &format!("+{NOTES}:{NOTES}")])
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn ids(records: &[serde_json::Value]) -> Vec<String> {
    records
        .iter()
        .map(|record| record["id"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_producer_cannot_drop_its_own_failure() {
    let (test_box, host, repo, keys, tree, commit) = landed();
    let landed_records = note(&test_box, &repo, &tree);
    assert!(!landed_records.is_empty(), "the landing left its evidence");

    // Adding a record still works: the producer's own signed failure on the landed tree.
    let failure = record(&keys, &tree, &commit, "rerun-failed", "product_failure");
    let mut with_failure = landed_records.clone();
    with_failure.push(failure.clone());
    write_note(&test_box, &repo, &tree, &with_failure);
    let (ok, why) = push(&test_box, &repo);
    assert!(ok, "adding a record is accepted: {why}");
    assert_eq!(note(&test_box, &host, &tree), with_failure);

    // A note that drops the failure, as a plain fast-forward notes commit: refused, named.
    write_note(&test_box, &repo, &tree, &landed_records);
    let (ok, why) = push(&test_box, &repo);
    assert!(!ok, "dropping a record was accepted");
    assert!(why.contains("EVIDENCE_WITHDRAWN"), "{why}");
    assert!(why.contains(&format!("{PRODUCER}/rerun-failed")), "{why}");
    assert_eq!(
        note(&test_box, &host, &tree),
        with_failure,
        "the host keeps the old note"
    );

    // Changing a record's bytes is a withdrawal of the old record too, even re-signed.
    let mut changed = with_failure.clone();
    *changed.last_mut().unwrap() = record(&keys, &tree, &commit, "rerun-failed", "pass");
    write_note(&test_box, &repo, &tree, &changed);
    let (ok, why) = push(&test_box, &repo);
    assert!(!ok, "changing a record was accepted");
    assert!(why.contains("EVIDENCE_WITHDRAWN"), "{why}");

    // Removing the whole note is refused as well.
    test_box.git_ok(
        &repo,
        &["notes", &format!("--ref={NOTES}"), "remove", &tree],
    );
    let (ok, why) = push(&test_box, &repo);
    assert!(!ok, "removing the note was accepted");
    assert!(why.contains("EVIDENCE_WITHDRAWN"), "{why}");
    assert_eq!(note(&test_box, &host, &tree), with_failure);
}

/// Runs `git <args>` in `dir` with `input` on stdin; returns its trimmed stdout.
fn git_in(test_box: &TestBox, dir: &Path, args: &[&str], input: &str) -> String {
    let mut child = test_box
        .git(dir)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// Git moves notes to fan-out paths (`ab/cdef…`) as a notes tree grows. A note that only moved
/// keeps every record: it is not a withdrawal.
#[test]
fn a_note_moved_to_a_fan_out_path_is_not_a_withdrawal() {
    let (test_box, host, repo, _, tree, _) = landed();
    let tip = test_box.git_ok(&repo, &["rev-parse", NOTES]);
    let listing = test_box.git_ok(&repo, &["ls-tree", &format!("{tip}^{{tree}}")]);
    let (mut entries, mut blob) = (String::new(), None);
    for line in listing.lines() {
        if line.ends_with(&format!("\t{tree}")) {
            blob = line.split_whitespace().nth(2).map(str::to_owned);
        } else {
            entries.push_str(line);
            entries.push('\n');
        }
    }
    let blob = blob.expect("a flat note on the landed tree");
    let inner = git_in(
        &test_box,
        &repo,
        &["mktree"],
        &format!("100644 blob {blob}\t{}\n", &tree[2..]),
    );
    entries.push_str("040000 tree ");
    entries.push_str(&inner);
    entries.push('\t');
    entries.push_str(&tree[..2]);
    entries.push('\n');
    let moved = git_in(&test_box, &repo, &["mktree"], &entries);
    let commit = test_box.git_ok(&repo, &["commit-tree", &moved, "-p", &tip, "-m", "fan-out"]);
    test_box.git_ok(&repo, &["update-ref", NOTES, &commit]);
    let (ok, why) = push(&test_box, &repo);
    assert!(ok, "a moved note keeps every record: {why}");
    assert_eq!(test_box.git_ok(&host, &["rev-parse", NOTES]), commit);
}

#[test]
fn a_rewrite_that_keeps_every_record_in_another_order_is_accepted() {
    let (test_box, host, repo, keys, tree, commit) = landed();
    let mut records = note(&test_box, &repo, &tree);
    records.push(record(&keys, &tree, &commit, "second", "pass"));
    write_note(&test_box, &repo, &tree, &records);
    let (ok, why) = push(&test_box, &repo);
    assert!(ok, "{why}");

    // The same records, reversed and in compact JSON: other note bytes, every record kept.
    records.reverse();
    let text = serde_json::to_string(&records).unwrap();
    test_box.git_ok(
        &repo,
        &[
            "notes",
            &format!("--ref={NOTES}"),
            "add",
            "-f",
            "-m",
            &text,
            &tree,
        ],
    );
    let (ok, why) = push(&test_box, &repo);
    assert!(ok, "a reordering keeps every record: {why}");
    assert_eq!(ids(&note(&test_box, &host, &tree)), ids(&records));
}

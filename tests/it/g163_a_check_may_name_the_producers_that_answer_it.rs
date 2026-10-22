//! G163. A check may name the producers that answer it (owner ruling Q3).
//!
//! `[[check]] producers = ["name", …]` is optional and judged at the base, inside the definition
//! digest. With it absent any producer answers. With it present, a record for that check from a
//! producer it does not list does not count, and the refusal names it `unlisted_producer`. A
//! candidate that edits the list is a policy change, checked by the base's policy first, so
//! widening the list for itself has no effect on its own landing (G26).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use bay::evidence::{self, EvidenceRecord};
use bay::git::Git;
use support::{TestBox, check_files, passing_result, result_json};

/// The producer name of a box that sets none (`bay.producerName`).
const OWN: &str = "local";
/// Another producer, the only one the base's list names.
const LISTED: &str = "gate-2";

/// A trunk whose one check lists `producers`, and a change whose view covers the policy.
fn change(producers: &str) -> (TestBox, PathBuf, String, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str("producers = ");
            text.push_str(producers);
            text.push('\n');
        }
    }
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let started = test_box.bay_ok(
        &repo,
        &["start", "CARD-1", "--paths", "src/", ".bay/", "scripts/"],
    );
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/new.rs"), "fn new() {}\n").unwrap();
    (test_box, repo, id, view)
}

/// The refusal's `unlisted_producer` entries, as (check, producer).
fn unlisted(blocked: &serde_json::Value) -> Vec<(String, String)> {
    blocked["detail"]["checks"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|row| row["why"] == "unlisted_producer")
        .map(|row| {
            (
                row["check"].as_str().unwrap().to_owned(),
                row["producer"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// This box's own records on the last attempt's tree of change `id`.
fn own_records(test_box: &TestBox, repo: &Path, id: &str) -> Vec<EvidenceRecord> {
    let change = test_box.git_ok(
        repo,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    let change: serde_json::Value = serde_json::from_str(&change).unwrap();
    let tree = change["operations"][0]["attempts"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["tree"]
        .as_str()
        .unwrap()
        .to_owned();
    evidence::producer_records(&Git::new(repo), OWN, &tree).unwrap()
}

#[test]
fn a_record_from_an_unlisted_producer_does_not_count() {
    let (test_box, repo, id, view) = change(&format!("[\"{LISTED}\"]"));
    // This box's producer is not listed: its own pass does not answer the check.
    let (code, blocked) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_UNKNOWN", "{blocked}");
    assert_eq!(
        unlisted(&blocked),
        vec![("tests".to_owned(), OWN.to_owned())],
        "{blocked}"
    );

    // The same pass from the listed producer answers it (no registry: records count unsigned).
    let mine = own_records(&test_box, &repo, &id);
    let pass = mine
        .iter()
        .find(|record| record.verdict == evidence::Verdict::Pass)
        .expect("this box's pass");
    let mut theirs = pass.clone();
    theirs.id = format!("{LISTED}/{}", pass.id.split_once('/').unwrap().1);
    theirs.producer.name = LISTED.to_owned();
    evidence::append(&Git::new(&repo), LISTED, &theirs).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged");
}

#[test]
fn a_listed_producer_answers_its_check() {
    let (test_box, _repo, _id, view) = change(&format!("[\"{LISTED}\", \"{OWN}\"]"));
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged");
}

#[test]
fn a_candidate_that_lists_its_own_producer_gets_no_effect() {
    let (test_box, _repo, _id, view) = change(&format!("[\"{LISTED}\"]"));
    // The candidate adds this box's producer to the list and raises the revision: the base's
    // list still judges it.
    let path = view.join(".bay/policy.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let widened = text.replace("revision = 1", "revision = 2").replace(
        &format!("producers = [\"{LISTED}\"]"),
        &format!("producers = [\"{LISTED}\", \"{OWN}\"]"),
    );
    assert_ne!(text, widened);
    std::fs::write(&path, widened).unwrap();
    let (code, blocked) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_UNKNOWN", "{blocked}");
    assert_eq!(
        unlisted(&blocked),
        vec![("tests".to_owned(), OWN.to_owned())],
        "{blocked}"
    );
}

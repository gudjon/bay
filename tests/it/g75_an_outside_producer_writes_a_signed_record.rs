//! G75: `bay evidence record` is the supported write path for an outside producer's record
//! (`INTERFACES` § 6; corrections K4 of the review dossier).
//!
//! One call writes one record per named landing and pushes them once. A record's id comes from
//! its writer, kind, subject commit and a digest of its inputs, so a rerun of the same bytes is
//! a no-op and a different record under that id is a conflict, refused. The verdict has no
//! default. Fields the record format does not know (a `findings` array) are kept inside the
//! signed bytes. A record over the size limit is refused, never cut. A `review` record is
//! signed by the box's reviewer key, and only by it (Q12).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use bay::evidence::{self, EvidenceRecord};
use bay::git::Git;
use support::talk::fresh_key;
use support::{
    AGENT, EXECUTOR, PRODUCER, REGISTRAR, TestBox, TestKeys, check_files, key_line, passing_result,
    result_json,
};

const REVIEWER: &str = "review-1";

/// A registry that enrols reviewer `review-1`, two landed changes, and a reviewer box configured
/// with that reviewer's key. Returns the box, the remote, the reviewer box, the two landings,
/// and the reviewer's key.
fn reviewed() -> (TestBox, PathBuf, PathBuf, [String; 2], PathBuf, TestKeys) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let keys = test_box.keys();
    let reviewer = fresh_key(&test_box, "reviewer");
    test_box.enroll_files(
        &repo,
        &keys,
        &[
            ("agent", key_line(AGENT, &keys.agent)),
            ("executor", key_line(EXECUTOR, &keys.executor)),
            ("producer", key_line(PRODUCER, &keys.producer)),
            ("reviewer", key_line(REVIEWER, &reviewer)),
            ("registry", key_line(REGISTRAR, &keys.registry)),
        ],
    );
    test_box.use_keys(&repo, &keys);
    let mut landed = Vec::new();
    for body in ["fn one() {}\n", "fn two() {}\n"] {
        let (_, view) = test_box.start_and_edit(&repo);
        std::fs::write(view.join("src/new.rs"), body).unwrap();
        let (code, result) = result_json(test_box.bay(&view).args(["land", "--wait"]));
        assert_eq!(code, 0, "{result}");
        landed.push(result["detail"]["commit"].as_str().unwrap().to_owned());
    }
    let box_dir = test_box.clone_of(&remote, "reviewer-box");
    test_box.git_ok(&box_dir, &["config", "bay.reviewerName", REVIEWER]);
    test_box.git_ok(
        &box_dir,
        &["config", "bay.reviewerKey", reviewer.to_str().unwrap()],
    );
    (
        test_box,
        remote,
        box_dir,
        landed.try_into().unwrap(),
        reviewer,
        keys,
    )
}

fn record(test_box: &TestBox, dir: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    result_json(test_box.bay(dir).args(["evidence", "record"]).args(args))
}

/// The reviewer's records on the remote, about `commit`.
fn on_remote(test_box: &TestBox, remote: &Path, commit: &str) -> Vec<EvidenceRecord> {
    let git = Git::new(remote);
    let tree = test_box.git_bare(remote, &["rev-parse", &format!("{commit}^{{tree}}")]);
    evidence::producer_records(&git, REVIEWER, &tree)
        .unwrap()
        .into_iter()
        .filter(|record| record.subject.commit == commit)
        .collect()
}

#[test]
fn a_review_with_findings_is_signed_whole_and_covers_its_landings() {
    let (test_box, remote, box_dir, [first, second], _, _) = reviewed();
    let findings = test_box.path().join("findings.json");
    std::fs::write(
        &findings,
        r#"{"findings": [{"path": "src/new.rs", "line": 1, "what": "unused function"}]}"#,
    )
    .unwrap();
    let (code, written) = record(
        &test_box,
        &box_dir,
        &[
            "--kind",
            "review",
            "--verdict",
            "product_failure",
            "--commit",
            &first,
            "--commit",
            &second,
            "--input",
            "model=any-model",
            "--body",
            findings.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{written}");
    assert_eq!(
        written["detail"]["records"].as_array().unwrap().len(),
        2,
        "{written}"
    );

    let records = on_remote(&test_box, &remote, &first);
    let [record] = records.as_slice() else {
        panic!("one review record on the first landing: {records:?}");
    };
    assert_eq!(record.kind, "review");
    assert_eq!(record.producer.name, REVIEWER);
    assert_eq!(record.unknown["findings"][0]["what"], "unused function");
    assert_eq!(record.inputs.as_ref().unwrap()["model"], "any-model");
    let registry = test_box.git_bare(&remote, &["rev-parse", "refs/bay/registry"]);
    let host = Git::new(&remote);
    let opened = bay::registry::Registry::load_at(&host, &registry).unwrap();
    assert!(
        record.verifies_in(&host, &opened).unwrap(),
        "signed by the reviewer"
    );
    let mut altered = record.clone();
    altered
        .unknown
        .insert("findings".to_owned(), serde_json::json!([]));
    assert!(
        !altered.verifies_in(&host, &opened).unwrap(),
        "the findings are inside the signed bytes"
    );
    assert_eq!(
        on_remote(&test_box, &remote, &second).len(),
        1,
        "one call, both landings"
    );

    // The reviewer's queue no longer holds them.
    let (_, missing) = result_json(test_box.bay(&box_dir).args([
        "evidence",
        "missing",
        "--kind",
        "review",
        "--producer",
        REVIEWER,
    ]));
    let listed: Vec<&str> = missing["detail"]["missing"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row["commit"].as_str())
        .collect();
    assert!(
        !listed.contains(&first.as_str()) && !listed.contains(&second.as_str()),
        "{missing}"
    );
}

#[test]
fn a_rerun_is_a_no_op_and_a_different_record_under_one_id_is_refused() {
    let (test_box, remote, box_dir, [first, _], _, _) = reviewed();
    let args = [
        "--kind",
        "review",
        "--verdict",
        "pass",
        "--commit",
        first.as_str(),
    ];
    let (code, once) = record(&test_box, &box_dir, &args);
    assert_eq!(code, 0, "{once}");
    let (code, again) = record(&test_box, &box_dir, &args);
    assert_eq!(code, 0, "{again}");
    assert_eq!(
        again["detail"]["records"], once["detail"]["records"],
        "{again}"
    );
    assert_eq!(
        on_remote(&test_box, &remote, &first).len(),
        1,
        "a rerun writes nothing new"
    );

    // Same writer, kind, commit and inputs, another verdict: a conflict, never a second record.
    let (code, conflict) = record(
        &test_box,
        &box_dir,
        &[
            "--kind",
            "review",
            "--verdict",
            "product_failure",
            "--commit",
            &first,
        ],
    );
    assert_ne!(code, 0, "{conflict}");
    let records = on_remote(&test_box, &remote, &first);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].verdict, evidence::Verdict::Pass);
}

#[test]
fn a_record_over_the_limit_is_refused_and_nothing_is_written() {
    let (test_box, remote, box_dir, [first, _], _, _) = reviewed();
    let body = test_box.path().join("big.json");
    std::fs::write(
        &body,
        serde_json::json!({"findings": ["x".repeat(70_000)]}).to_string(),
    )
    .unwrap();
    let (code, refused) = record(
        &test_box,
        &box_dir,
        &[
            "--kind",
            "review",
            "--verdict",
            "pass",
            "--commit",
            &first,
            "--body",
            body.to_str().unwrap(),
        ],
    );
    assert_ne!(code, 0, "{refused}");
    assert!(refused.to_string().contains("limit"), "{refused}");
    assert_eq!(on_remote(&test_box, &remote, &first).len(), 0);
}

#[test]
fn a_review_needs_the_reviewer_key_and_a_verdict() {
    let (test_box, remote, box_dir, [first, _], _, _) = reviewed();
    test_box.git_ok(&box_dir, &["config", "--unset", "bay.reviewerKey"]);
    let (code, refused) = record(
        &test_box,
        &box_dir,
        &["--kind", "review", "--verdict", "pass", "--commit", &first],
    );
    assert_ne!(code, 0, "{refused}");
    assert!(refused.to_string().contains("bay.reviewerKey"), "{refused}");
    assert_eq!(on_remote(&test_box, &remote, &first).len(), 0);
    // No verdict, no record: there is no default.
    let out = test_box
        .bay(&box_dir)
        .args(["evidence", "record", "--kind", "review", "--commit", &first])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

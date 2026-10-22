//! G86: a reader refuses a schema major it does not know (`INTERFACES` header).
//!
//! A record a later bay writes under `bay.evidence/2` may mean something this version cannot
//! judge, so it never counts as evidence here, even when its producer signed it: unknown is
//! never pass. The record stays on its note for the bay that knows it.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use bay::evidence::{self, EvidenceRecord};
use bay::git::Git;
use support::{PRODUCER, TestBox, check_files, passing_result, result_json};

#[test]
fn an_evidence_record_of_an_unknown_major_never_counts() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let commit = landed["detail"]["commit"].as_str().unwrap();
    let tree = test_box.git_ok(&repo, &["rev-parse", &format!("{commit}^{{tree}}")]);
    let git = Git::new(&repo);
    let [record] = evidence::producer_records(&git, PRODUCER, &tree)
        .unwrap()
        .try_into()
        .unwrap();

    // The same record as a later bay would write it, signed by the same producer.
    let mut value = serde_json::to_value(&record).unwrap();
    value["schema"] = serde_json::json!("bay.evidence/2");
    value["id"] = serde_json::json!(format!("{}-2", record.id.strip_suffix("-1").unwrap()));
    value.as_object_mut().unwrap().remove("signature");
    let payload = bay::canonical::canonical(&value).unwrap();
    value["signature"] = serde_json::json!(
        bay::signing::sign_data(&keys.producer, "bay-producer", payload.as_bytes()).unwrap()
    );
    let later: EvidenceRecord = serde_json::from_value(value).unwrap();
    evidence::append(&git, PRODUCER, &later).unwrap();

    let read = evidence::records_for(&git, &tree).unwrap();
    assert_eq!(
        read.len(),
        1,
        "only the record this bay can judge: {read:?}"
    );
    assert_eq!(read[0].schema, evidence::EVIDENCE_SCHEMA);
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let records = &status["detail"]["changes"][0]["evidence"]["records"];
    assert_eq!(records.as_array().unwrap().len(), 1, "{records}");
    // It stays on the note for the bay that knows it.
    assert_eq!(
        evidence::producer_records(&git, PRODUCER, &tree)
            .unwrap()
            .len(),
        2
    );
}

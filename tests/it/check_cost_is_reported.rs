//! Check-cost reporting (`ROADMAP` § 2 increment 3, § 3; `INTERFACES` § 3).
//!
//! A unit's `seconds` from the adapter's result is kept in the signed evidence record, beside
//! the wall time bay measured around the adapter run; `bay status` sums both per operation and
//! per landing. A record written before those fields existed still verifies and still counts.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use bay::evidence::{self, EvidenceRecord};
use bay::git::Git;
use support::{PRODUCER, TestBox, check_files, result_json};

/// A check result with one passing unit that reports `seconds`.
fn timed_result(seconds: &str) -> String {
    format!(
        "printf '%s' \"{{\\\"schema\\\":\\\"bay.check.result/1\\\",\\\"exercised\\\":{{\\\"commit\\\":\\\"$commit\\\",\\\"tree\\\":\\\"$tree\\\"}},\\\"plan\\\":{{\\\"discovered\\\":1,\\\"selected\\\":1,\\\"completed\\\":1}},\\\"checks\\\":[{{\\\"id\\\":\\\"t1\\\",\\\"verdict\\\":\\\"pass\\\",\\\"seconds\\\":{seconds}}}]}}\""
    )
}

#[test]
fn a_units_seconds_reach_the_signed_record_and_the_status_sum() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &timed_result("4.1"), 1, 60),
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
    // Read as JSON, as any reader of the note would.
    let note = test_box.git_ok(
        &repo,
        &[
            "notes",
            &format!("--ref=refs/notes/bay/evidence/{PRODUCER}"),
            "show",
            &tree,
        ],
    );
    let records: Vec<serde_json::Value> = serde_json::from_str(&note).unwrap();
    let [json] = records.as_slice() else {
        panic!("one record on the landed tree: {note}");
    };
    assert_eq!(json["units"][0]["seconds"], 4.1, "{note}");
    let measured = json["seconds"].as_f64();
    assert!(
        measured.is_some_and(|seconds| seconds >= 0.0),
        "bay measured the adapter run: {note}"
    );
    let record: EvidenceRecord = serde_json::from_value(json.clone()).unwrap();
    let producers = repo
        .join(".git/bay/registry")
        .join(test_box.git_ok(&repo, &["rev-parse", "refs/bay/registry"]))
        .join("producer");
    assert!(record.verifies(&producers), "the times are signed");
    let mut altered = json.clone();
    altered["units"][0]["seconds"] = serde_json::json!(0.1);
    let altered: EvidenceRecord = serde_json::from_value(altered).unwrap();
    assert!(
        !altered.verifies(&producers),
        "a changed time breaks the signature"
    );

    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    let operation_cost = &change["operation"]["cost"];
    assert_eq!(operation_cost["unit_seconds"], 4.1, "{change}");
    assert_eq!(operation_cost["records"], 1, "{change}");
    assert_eq!(operation_cost["untimed"], 0, "{change}");
    assert_eq!(
        operation_cost["worker_seconds"].as_f64(),
        measured,
        "{change}"
    );
    assert_eq!(change["landing_cost"]["unit_seconds"], 4.1, "{change}");
    assert_eq!(change["landing_cost"]["records"], 1, "{change}");

    // A record in the shape written before the times existed: signed by the producer over the
    // canonical JSON without them. It still verifies, and status counts it.
    let mut value = json.clone();
    let old_id = format!("{}-2", record.id.strip_suffix("-1").unwrap());
    let object = value.as_object_mut().unwrap();
    object.insert("id".to_owned(), serde_json::json!(old_id));
    object.remove("seconds");
    object.remove("signature");
    value["units"][0].as_object_mut().unwrap().remove("seconds");
    let payload = bay::canonical::canonical(&value).unwrap();
    assert!(!payload.contains("seconds"), "{payload}");
    let signature =
        bay::signing::sign_data(&keys.producer, "bay-producer", payload.as_bytes()).unwrap();
    value["signature"] = serde_json::json!(signature);
    let old: EvidenceRecord = serde_json::from_value(value).unwrap();
    assert!(old.verifies(&producers), "a record without times verifies");
    evidence::append(&git, PRODUCER, &old).unwrap();

    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    let operation_cost = &change["operation"]["cost"];
    assert_eq!(operation_cost["records"], 2, "{change}");
    assert_eq!(operation_cost["untimed"], 1, "{change}");
    assert_eq!(operation_cost["unit_seconds"], 4.1, "{change}");
    assert_eq!(change["landing_cost"]["records"], 2, "{change}");
}

/// Evidence readers keep fields they do not know and verify over them, so a field a later
/// version adds never breaks a signature an older reader checks.
#[test]
fn a_record_with_fields_this_version_does_not_know_verifies_and_counts() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &timed_result("1"), 1, 60),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let commit = landed["detail"]["commit"].as_str().unwrap();
    let tree = test_box.git_ok(&repo, &["rev-parse", &format!("{commit}^{{tree}}")]);
    let note = test_box.git_ok(
        &repo,
        &[
            "notes",
            &format!("--ref=refs/notes/bay/evidence/{PRODUCER}"),
            "show",
            &tree,
        ],
    );
    let records: Vec<serde_json::Value> = serde_json::from_str(&note).unwrap();
    let producers = repo
        .join(".git/bay/registry")
        .join(test_box.git_ok(&repo, &["rev-parse", "refs/bay/registry"]))
        .join("producer");

    // A later version's record: unknown fields at the top, in a unit and in each nested
    // object, signed by the producer over the canonical JSON that holds them.
    let mut future = records[0].clone();
    let base_id = future["id"].as_str().unwrap().to_owned();
    let future_id = format!("{}-2", base_id.strip_suffix("-1").unwrap());
    future["id"] = serde_json::json!(future_id);
    future["future"] = serde_json::json!(1);
    future["units"][0]["future"] = serde_json::json!({"nested": [1.5, "x"]});
    future["subject"]["future"] = serde_json::json!(true);
    future["check"]["future"] = serde_json::json!("c");
    future["producer"]["future"] = serde_json::json!(null);
    future.as_object_mut().unwrap().remove("signature");
    let payload = bay::canonical::canonical(&future).unwrap();
    future["signature"] = serde_json::json!(
        bay::signing::sign_data(&keys.producer, "bay-producer", payload.as_bytes()).unwrap()
    );
    let read: EvidenceRecord = serde_json::from_value(future.clone()).unwrap();
    assert!(
        read.verifies(&producers),
        "unknown fields are verified over: signed {payload}, read back {}",
        String::from_utf8(read.signing_payload().unwrap()).unwrap()
    );

    // The same record with an unknown field changed after signing does not verify.
    let mut altered = future.clone();
    altered["units"][0]["future"] = serde_json::json!({"nested": [2.5, "x"]});
    let altered: EvidenceRecord = serde_json::from_value(altered).unwrap();
    assert!(
        !altered.verifies(&producers),
        "an altered unknown field breaks it"
    );

    // Appended and read back through a note, it still counts.
    let git = Git::new(&repo);
    evidence::append(&git, PRODUCER, &read).unwrap();
    let back = evidence::producer_records(&git, PRODUCER, &tree).unwrap();
    let kept = back.iter().find(|record| record.id == future_id).unwrap();
    assert!(kept.verifies(&producers), "a re-read record still verifies");
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    assert_eq!(change["operation"]["cost"]["records"], 2, "{change}");
    assert_eq!(change["operation"]["cost"]["unit_seconds"], 2.0, "{change}");
}

//! G240: a failed check says why (`INTERFACES` § 3).
//!
//! A unit of `bay.check.result/1` may carry a `reason`, at most 2 KB of UTF-8, that the
//! adapter chose to show. bay copies it into the signed evidence record and shows it in the
//! refusal's `detail.checks` and in `bay status`. A longer reason makes the run
//! `infra_failure`: bay never cuts it, and never copies a check's raw output.

// Each case runs a shell adapter, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use bay::git::Git;
use support::{PRODUCER, TestBox, check_files, result_json};

/// A result with one passing unit and one failing unit whose reason is `reason` (a shell word).
fn failing_result(reason: &str) -> String {
    format!(
        "printf '%s' \"{{\\\"schema\\\":\\\"bay.check.result/1\\\",\\\"exercised\\\":{{\\\"commit\\\":\\\"$commit\\\",\\\"tree\\\":\\\"$tree\\\"}},\\\"plan\\\":{{\\\"discovered\\\":2,\\\"selected\\\":2,\\\"completed\\\":2}},\\\"checks\\\":[{{\\\"id\\\":\\\"t1\\\",\\\"verdict\\\":\\\"pass\\\"}},{{\\\"id\\\":\\\"t2\\\",\\\"verdict\\\":\\\"product_failure\\\",\\\"reason\\\":\\\"{reason}\\\"}}]}}\""
    )
}

#[test]
fn a_failed_units_reason_reaches_the_refusal_the_signed_record_and_status() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("", &failing_result("AGENTS.md: missing owner line"), 2, 60),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    let check = &refused["detail"]["checks"][0];
    assert_eq!(
        check["reasons"],
        serde_json::json!([{"unit": "t2", "verdict": "product_failure",
            "reason": "AGENTS.md: missing owner line"}]),
        "only the failed unit's reason, as the adapter wrote it: {refused}"
    );

    // The reason is in the signed record: changing it breaks the signature.
    let git = Git::new(&repo);
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let evidence = &status["detail"]["changes"][0]["evidence"];
    let tree = evidence["tree"].as_str().unwrap();
    let records = bay::evidence::producer_records(&git, PRODUCER, tree).unwrap();
    let [record] = records.as_slice() else {
        panic!("one record on the candidate tree: {records:?}");
    };
    assert_eq!(
        record.units[1].reason.as_deref(),
        Some("AGENTS.md: missing owner line")
    );
    assert_eq!(record.units[0].reason, None, "a pass carries no reason");
    let producers = repo
        .join(".git/bay/registry")
        .join(test_box.git_ok(&repo, &["rev-parse", "refs/bay/registry"]))
        .join("producer");
    assert!(record.verifies(&producers), "the reason is signed");
    let mut altered = record.clone();
    altered.units[1].reason = Some("nothing wrong".to_owned());
    assert!(
        !altered.verifies(&producers),
        "a changed reason breaks the signature"
    );

    assert_eq!(
        evidence["records"][0]["reasons"], check["reasons"],
        "status shows the same reasons: {evidence}"
    );
}

#[test]
fn a_reason_over_two_kilobytes_is_an_infra_failure_never_cut() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(
            "long=$(head -c 2049 /dev/zero | tr '\\000' x)",
            &failing_result("$long"),
            2,
            60,
        ),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_ne!(code, 0, "{refused}");
    assert_eq!(refused["reason"], "CHECK_UNKNOWN", "{refused}");
    let check = &refused["detail"]["checks"][0];
    assert_eq!(check["verdict"], "infra_failure", "{refused}");
    assert!(
        check["why"]
            .as_str()
            .is_some_and(|why| why.contains("2049 bytes") && why.contains("t2")),
        "the refusal names the unit and the size: {refused}"
    );
    assert!(
        !refused.to_string().contains(&"x".repeat(2049)),
        "the over-long reason is never shown"
    );
}

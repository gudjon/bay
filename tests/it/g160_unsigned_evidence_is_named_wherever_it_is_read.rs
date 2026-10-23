//! G160. Unsigned evidence is named wherever it is read.
//!
//! Without a registry bay runs unsigned and every check record counts, so a result that reports
//! evidence says so: the land result and `bay status` carry `trust: "unsigned"`, and each record
//! status lists carries `verified: "unsigned"`. With a registry they carry `trust: "signed"`, and
//! each record `verified` or `unverifiable`. A completion profile that sets
//! `require_signed = true` (judged at the base, like the profile) never counts unsigned
//! records: a dimension that rests only on them is `unknown`, never satisfied (owner ruling Q6).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use bay::evidence::{self, EvidenceRecord};
use bay::git::Git;
use support::{PRODUCER, TestBox, TestKeys, check_files, passing_result, result_json};

const STRICT: &str =
    "\n[profile.strict]\nrequire = [\"merged\", \"checked\"]\nrequire_signed = true\n";
const SELECT_STRICT: &str = "[policy]\nprofile = \"strict\"\n";

/// A box with one change landed under `policy_extra` and `config`, with a registry enrolled
/// only when `enroll`. Returns the box, the repo, its keys, the change id and the land result.
fn land_one(
    policy_extra: &str,
    config: &str,
    enroll: bool,
) -> (TestBox, PathBuf, TestKeys, String, serde_json::Value) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(policy_extra);
        }
    }
    if !config.is_empty() {
        files.push((".bay/config.toml".to_owned(), config.to_owned()));
    }
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    if enroll {
        test_box.enroll(&repo, &keys);
        test_box.use_keys(&repo, &keys);
    }
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    (test_box, repo, keys, id, landed)
}

fn change_of(test_box: &TestBox, repo: &std::path::Path, id: &str) -> serde_json::Value {
    let status = test_box.bay_ok(repo, &["status", id]);
    status["detail"]["changes"][0].clone()
}

#[test]
fn without_a_registry_every_result_names_the_evidence_unsigned() {
    let (test_box, repo, _, id, landed) = land_one("", "", false);
    assert_eq!(landed["detail"]["trust"], "unsigned", "{landed}");

    let change = change_of(&test_box, &repo, &id);
    let evidence = &change["evidence"];
    assert_eq!(evidence["trust"], "unsigned", "{change}");
    let records = evidence["records"].as_array().unwrap();
    assert!(!records.is_empty(), "{change}");
    for record in records {
        assert_eq!(record["verified"], "unsigned", "{record}");
    }
    // Unsigned check records still count by default (owner ruling Q6).
    assert_eq!(change["completion"]["checked"], true, "{change}");
    assert_eq!(
        change["completion"]["profile"]["verdict"], "complete",
        "{change}"
    );
}

#[test]
fn with_a_registry_records_are_named_verified_or_unverifiable() {
    let (test_box, repo, keys, id, landed) = land_one("", "", true);
    assert_eq!(landed["detail"]["trust"], "signed", "{landed}");

    // An agent-signed record under the producer's name, on the landed tree.
    let commit = landed["detail"]["commit"].as_str().unwrap();
    let tree = test_box.git_ok(&repo, &["rev-parse", &format!("{commit}^{{tree}}")]);
    let git = Git::new(&repo);
    let mut forged: EvidenceRecord = evidence::producer_records(&git, PRODUCER, &tree)
        .unwrap()
        .into_iter()
        .next()
        .expect("the check's record");
    forged.id = format!("{PRODUCER}/forged");
    forged.sign(&keys.agent).unwrap();
    evidence::append(&git, PRODUCER, &forged).unwrap();

    let change = change_of(&test_box, &repo, &id);
    let evidence = &change["evidence"];
    assert_eq!(evidence["trust"], "signed", "{change}");
    let verified = |wanted: &str| {
        evidence["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["id"] == wanted)
            .map(|record| record["verified"].clone())
    };
    assert_eq!(
        verified(&format!("{PRODUCER}/forged")),
        Some(serde_json::json!("unverifiable")),
        "{change}"
    );
    let signed = evidence["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["verified"] == "verified")
        .count();
    assert!(signed >= 1, "{change}");
}

#[test]
fn a_profile_that_requires_signed_evidence_never_counts_unsigned_records() {
    let (test_box, repo, _, id, landed) = land_one(STRICT, SELECT_STRICT, false);
    assert_eq!(landed["detail"]["trust"], "unsigned", "{landed}");
    let change = change_of(&test_box, &repo, &id);
    let profile = &change["completion"]["profile"];
    assert_eq!(profile["name"], "strict", "{change}");
    assert_eq!(profile["require_signed"], true, "{change}");
    assert_eq!(profile["verdict"], "unknown", "{change}");
    assert_eq!(
        profile["unsigned"],
        serde_json::json!(["checked"]),
        "{change}"
    );
    assert!(
        profile["summary"].as_str().unwrap().contains("unsigned"),
        "{change}"
    );

    // Under a registry the same profile is satisfied by producer-signed records.
    let (test_box, repo, _, id, _) = land_one(STRICT, SELECT_STRICT, true);
    let change = change_of(&test_box, &repo, &id);
    let profile = &change["completion"]["profile"];
    assert_eq!(profile["verdict"], "complete", "{change}");
    assert_eq!(profile["unsigned"], serde_json::json!([]), "{change}");
}

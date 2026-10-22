//! G16. Identity is verifiable.
//!
//! A publication or evidence record signed with a key outside the class file it must verify
//! against is reported unverifiable and never counts as evidence; a `pass` evidence note
//! signed with an agent key (not a producer key) is refused as evidence.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use bay::evidence::{self, EvidenceRecord, Verdict};
use bay::git::Git;
use support::{AGENT, PRODUCER, TestBox, check_files, passing_result, result_json};

fn signed_box(
    check: &[(String, String)],
) -> (
    TestBox,
    std::path::PathBuf,
    std::path::PathBuf,
    support::TestKeys,
) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], check);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    (test_box, repo, remote, keys)
}

#[test]
fn a_signed_landing_verifies_against_the_executor_file_of_its_registry() {
    let (test_box, repo, remote, keys) =
        signed_box(&check_files("true", &passing_result(1), 1, 60));
    let registry = test_box.git_ok(&repo, &["rev-parse", "refs/bay/registry"]);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");

    let commit = landed["detail"]["commit"].as_str().unwrap();
    assert!(test_box.executor_signed(&remote, &keys, commit));
    let trailers = test_box.git_bare(
        &remote,
        &[
            "log",
            "-1",
            "--format=%(trailers:key=Registry-Id,valueonly)",
            commit,
        ],
    );
    assert_eq!(trailers.trim(), registry);
    let tree = test_box.git_bare(&remote, &["rev-parse", &format!("{commit}^{{tree}}")]);
    let note = test_box.git_bare(
        &remote,
        &[
            &format!("--work-tree={}", repo.display()),
            "notes",
            &format!("--ref=refs/notes/bay/evidence/{PRODUCER}"),
            "show",
            &tree,
        ],
    );
    assert!(note.contains("\"signature\""), "{note}");
    let status = test_box.bay_ok(&repo, &["status", &id]);
    assert_eq!(status["detail"]["changes"][0]["state"], "landed");
    assert_eq!(
        status["detail"]["changes"][0]["anomalies"],
        serde_json::json!([])
    );
}

/// Pushes onto trunk a commit that quotes the change's trailers, signed with `key` and naming
/// `committer`. Returns the commit.
fn forge(
    test_box: &TestBox,
    repo: &Path,
    id: &str,
    registry: &str,
    key: &Path,
    committer: &str,
) -> String {
    test_box.git_ok(repo, &["fetch", "-q", "origin"]);
    let trunk = test_box.git_ok(repo, &["rev-parse", "origin/main"]);
    let tree = test_box.git_ok(repo, &["rev-parse", "origin/main^{tree}"]);
    let message =
        format!("forged\n\nChange-Id: {id}\nRevision-Id: {trunk}\nRegistry-Id: {registry}\n");
    let out = test_box
        .git(repo)
        .env("GIT_COMMITTER_NAME", committer)
        .args(["-c", "gpg.format=ssh", "-c"])
        .arg(format!("user.signingkey={}", key.display()))
        .args(["commit-tree", "-S", &tree, "-p", &trunk, "-m", &message])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let forged = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    test_box.git_ok(
        repo,
        &["push", "-q", "origin", &format!("{forged}:refs/heads/main")],
    );
    forged
}

#[test]
fn a_publication_signed_outside_the_executor_file_is_an_anomaly_not_a_landing() {
    let (test_box, repo, remote, keys) =
        signed_box(&check_files("true", &passing_result(1), 1, 60));
    let registry = test_box.git_ok(&repo, &["rev-parse", "refs/bay/registry"]);
    let (id, _) = test_box.start_and_edit(&repo);

    // The agent signs a "publication" under its own name: its key is in the agent file, not
    // the executor file.
    let by_agent = forge(&test_box, &repo, &id, &registry, &keys.agent, AGENT);
    // An executor key signs a commit that names another executor: the signature's principal
    // must equal the committer it names (DESIGN § 10a).
    let misnamed = forge(&test_box, &repo, &id, &registry, &keys.executor, "exec-2");

    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    assert_ne!(change["state"], "landed", "{status}");
    let anomalies: Vec<&str> = change["anomalies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|anomaly| anomaly["commit"].as_str().unwrap())
        .collect();
    assert!(anomalies.contains(&by_agent.as_str()), "{status}");
    assert!(anomalies.contains(&misnamed.as_str()), "{status}");

    // The real landing still goes through, and only it counts.
    let view = Path::new(&repo).join(".git/bay/views").join(&id);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let tip = test_box.git_bare(&remote, &["rev-parse", "main"]);
    assert_eq!(landed["detail"]["commit"], tip.as_str());
    assert!(test_box.executor_signed(&remote, &keys, &tip));
    assert!(
        !test_box.executor_signed(&remote, &keys, &by_agent),
        "the agent-signed commit does not verify as an executor's"
    );
}

/// A `pass` the check never produced, for the candidate tree, under producer `ci-1`.
fn forged_pass(template: &EvidenceRecord) -> EvidenceRecord {
    let mut record = template.clone();
    record.id = format!("{PRODUCER}/forged");
    record.verdict = Verdict::Pass;
    record.why = None;
    record.units = Vec::new();
    record.signature = None;
    record
}

#[test]
fn a_pass_signed_with_an_agent_key_is_refused_as_evidence() {
    // The check collects nothing, so only forged evidence could make it pass.
    let (test_box, repo, _remote, keys) =
        signed_box(&check_files("true", &passing_result(0), 1, 60));
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, blocked) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_UNKNOWN");

    let git = Git::new(&repo);
    let record_json = test_box.git_ok(
        &repo,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    let record: serde_json::Value = serde_json::from_str(&record_json).unwrap();
    let tree = record["operations"][0]["attempts"][0]["tree"]
        .as_str()
        .unwrap()
        .to_owned();
    let real = evidence::producer_records(&git, PRODUCER, &tree).unwrap();
    let template = real
        .first()
        .expect("the producer signed the unknown record");
    assert!(
        template.verifies(
            &repo
                .join(".git/bay/registry")
                .join(test_box.git_ok(&repo, &["rev-parse", "refs/bay/registry"]))
                .join("producer")
        )
    );

    // Unsigned, then agent-signed: neither counts.
    let unsigned = forged_pass(template);
    evidence::append(&git, PRODUCER, &unsigned).unwrap();
    let mut agent_signed = forged_pass(template);
    agent_signed.id = format!("{PRODUCER}/forged-by-agent");
    agent_signed.sign(&keys.agent).unwrap();
    evidence::append(&git, PRODUCER, &agent_signed).unwrap();
    let (code, still) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{still}");
    assert_eq!(still["reason"], "CHECK_UNKNOWN");

    // Control: the same pass under the producer key does count.
    let mut producer_signed = forged_pass(template);
    producer_signed.id = format!("{PRODUCER}/signed-by-producer");
    producer_signed.sign(&keys.producer).unwrap();
    evidence::append(&git, PRODUCER, &producer_signed).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged");
}

//! G91. Post-publication checks stay pending.
//!
//! A `[[check]]` with `stage = "post"`, judged in the policy at the base, never blocks a
//! landing: the pre-publication checks alone decide it. After the landing the post check is
//! shown pending for the landed commit, in the land result and in `status`, until a record for
//! that commit's tree arrives; then it shows that record's verdict. One check id in both stages
//! makes the policy invalid.

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

/// One `[[check]]` entry on the shared adapter, with `extra` lines.
fn entry(id: &str, extra: &str) -> String {
    format!(
        "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\n{extra}"
    )
}

/// A trunk under `policy` whose adapter passes `tests` and fails `suite` on every candidate:
/// the long suite is red, so only its stage keeps it from blocking.
fn trunk(policy: &str) -> (TestBox, std::path::PathBuf, std::path::PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let script = check_files(
        &format!("[ \"$1\" = suite ] && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    )
    .into_iter()
    .find(|(path, _)| path == "scripts/bay-check")
    .unwrap()
    .1;
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            ("scripts/bay-check".to_owned(), script),
            (".bay/policy.toml".to_owned(), policy.to_owned()),
        ],
    );
    (test_box, repo, remote)
}

/// The check ids of the records on `tree` in the remote, sorted.
fn checks_on(test_box: &TestBox, remote: &std::path::Path, tree: &str) -> Vec<String> {
    let notes = test_box.git_bare(
        remote,
        &["notes", "--ref=refs/notes/bay/evidence/local", "show", tree],
    );
    let records: Vec<serde_json::Value> = serde_json::from_str(&notes).unwrap();
    let mut ids: Vec<String> = records
        .iter()
        .map(|record| record["check"]["id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

#[test]
fn a_post_check_never_blocks_and_stays_pending_until_its_record_arrives() {
    let policy = format!(
        "revision = 1\n{}{}",
        entry("tests", ""),
        entry("suite", "stage = \"post\"\n")
    );
    let (test_box, repo, remote) = trunk(&policy);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "a red post check never blocks: {landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let commit = landed["detail"]["commit"].as_str().unwrap().to_owned();
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), commit);
    // The land result names the post check, pending for the landed commit.
    assert_eq!(
        landed["detail"]["post_checks"],
        serde_json::json!([{"check": "suite", "environment": "local", "state": "pending"}]),
        "{landed}"
    );
    // The landing ran only the pre-publication check: the post check is left to the runner
    // after publication.
    let tree = test_box.git_bare(&remote, &["rev-parse", &format!("{commit}^{{tree}}")]);
    assert_eq!(checks_on(&test_box, &remote, &tree), ["tests"]);

    let post = |test_box: &TestBox| {
        let status = test_box.bay_ok(&repo, &["status", &id]);
        status["detail"]["changes"][0]["post_checks"].clone()
    };
    assert_eq!(
        post(&test_box),
        serde_json::json!([{"check": "suite", "environment": "local", "state": "pending"}])
    );

    // A runner after publication records the suite on the landed tree, under its own producer
    // name, and pushes it: status then shows the record's verdict.
    let runner = test_box.clone_of(&remote, "runner");
    test_box.git_ok(
        &runner,
        &[
            "fetch",
            "-q",
            "origin",
            "refs/notes/bay/evidence/local:refs/notes/bay/evidence/local",
        ],
    );
    let notes = test_box.git_ok(
        &runner,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/local",
            "show",
            &tree,
        ],
    );
    let records: Vec<serde_json::Value> = serde_json::from_str(&notes).unwrap();
    let mut record = records[0].clone();
    record["id"] = serde_json::json!("tip/after-1");
    record["producer"]["name"] = serde_json::json!("tip");
    record["check"]["id"] = serde_json::json!("suite");
    record["verdict"] = serde_json::json!("product_failure");
    let text = serde_json::to_string(&serde_json::json!([record])).unwrap();
    test_box.git_ok(
        &runner,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/tip",
            "add",
            "-f",
            "-m",
            &text,
            &tree,
        ],
    );
    test_box.git_ok(
        &runner,
        &[
            "push",
            "-q",
            "origin",
            "refs/notes/bay/evidence/tip:refs/notes/bay/evidence/tip",
        ],
    );
    assert_eq!(
        post(&test_box),
        serde_json::json!([{"check": "suite", "environment": "local", "state": "product_failure"}])
    );
    // A red post check is a fact about the landed commit; it never unlands it.
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), commit);
}

#[test]
fn one_check_id_in_both_stages_makes_the_policy_invalid() {
    let mut post = entry("tests", "stage = \"post\"\n");
    post = post.replace("environment = \"local\"", "environment = \"arm\"");
    let policy = format!("revision = 1\n{}{post}", entry("tests", ""));
    let (test_box, repo, _) = trunk(&policy);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
    assert!(
        refused.to_string().contains("tests"),
        "the error names the check: {refused}"
    );
}

#[test]
fn an_unknown_stage_makes_the_policy_invalid() {
    let policy = format!(
        "revision = 1\n{}{}",
        entry("tests", ""),
        entry("suite", "stage = \"later\"\n")
    );
    let (test_box, repo, _) = trunk(&policy);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
}

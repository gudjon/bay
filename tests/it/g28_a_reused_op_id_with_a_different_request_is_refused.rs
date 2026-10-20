//! G28. A reused op id with a different request is refused.
//!
//! The same op id is sent with a different request digest: refused, no new work.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use bay::change::Chain;
use bay::git::Git;
use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn an_op_id_whose_request_digest_changed_is_refused_without_work() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    let trunk = test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(
            &test_box.relay(&format!("echo run >> '{}'", runs.display())),
            &passing_result(1),
            1,
            60,
        ),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0);

    // The same op id now carries a different request digest.
    let git = Git::new(&repo);
    let chain = Chain::new(&git);
    let (head, mut record) = chain.read(&id).unwrap();
    record.operations[0].request_digest = "sha256:not-the-request".to_owned();
    let draft = chain.drafts(&id).unwrap()[0].clone();
    chain
        .append(
            &head,
            &record,
            &draft,
            "a different request under the same op id",
        )
        .unwrap();
    test_box.git_ok(
        &repo,
        &["push", "-q", "origin", &format!("refs/bay/changes/{id}")],
    );

    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    let result = &ran["detail"]["ran"][0];
    assert_eq!(result["decision"], "failed", "{ran}");
    assert!(
        result["detail"]
            .as_str()
            .unwrap()
            .contains("reused with a different request digest"),
        "{ran}"
    );
    assert!(!runs.exists(), "no check ran");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
}

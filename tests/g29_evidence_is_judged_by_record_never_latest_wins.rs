//! G29. Evidence is judged by record, never latest-wins.
//!
//! The combine rules (an older pass with a newer `unknown` or `infra_failure` is pass; a pass and a
//! `product_failure` in either order is flaky; one environment does not satisfy another; a
//! commit-keyed failure is not charged to another commit; two records under one id are both
//! refused) are unit tests of `evidence.rs`; a result for a different commit is G3. Here: a
//! check that tries to read the producer key or write evidence from inside its sandbox
//! produces no counted evidence; one tree rechecked by another operation appends records
//! (reuse is off); one producer name used by two executors is reported as a collision.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_check_cannot_read_the_producer_key_or_write_evidence() {
    if support::sandbox().is_none() {
        eprintln!("no check sandbox on this machine; G29's sandbox case cannot run");
        return;
    }
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let keys = test_box.keys();
    let before = format!(
        "cat '{key}' > /dev/null; git -C '{repo}' notes --ref=refs/notes/bay/evidence/{producer} add -f -m forged HEAD",
        key = keys.producer.display(),
        repo = repo.display(),
        producer = support::PRODUCER,
    );
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(&before, &passing_result(1), 1, 60),
    );
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, blocked) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_UNKNOWN");
    assert_eq!(
        blocked["detail"]["checks"][0]["verdict"], "infra_failure",
        "{blocked}"
    );
    let notes = test_box.git_ok(
        &repo,
        &[
            "log",
            "--format=%B",
            &format!("refs/notes/bay/evidence/{}", support::PRODUCER),
        ],
    );
    assert!(!notes.contains("forged"), "{notes}");
}

#[test]
fn another_operation_on_the_same_tree_is_rechecked_not_reused() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let runs = test_box.path().join("runs");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files(
            &format!("echo run >> '{}'", runs.display()),
            &passing_result(1),
            1,
            60,
        ),
    );
    // Two changes make the same edit: the second candidate has the first one's tree.
    let (_, first) = test_box.start_and_edit(&repo);
    let (_, second) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&first).args(["land", "--wait"]));
    assert_eq!(code, 0);
    let tree = test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]);
    let (code, landed) = result_json(test_box.bay(&second).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main^{tree}"]),
        tree,
        "the same tree"
    );
    assert_eq!(
        std::fs::read_to_string(&runs).unwrap().lines().count(),
        2,
        "rechecked, not reused"
    );
    let notes = test_box.git_bare(
        &remote,
        &[
            "notes",
            "--ref=refs/notes/bay/evidence/local",
            "show",
            &tree,
        ],
    );
    let records: Vec<serde_json::Value> = serde_json::from_str(&notes).unwrap();
    assert_eq!(records.len(), 2, "the recheck appended its record");
}

#[test]
fn one_producer_name_used_by_two_executors_is_a_collision() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let one = test_box.clone_of(&remote, "executor-one");
    let two = test_box.clone_of(&remote, "executor-two");
    for (clone, name) in [(&one, "exec-one"), (&two, "exec-two")] {
        test_box.git_ok(clone, &["config", "bay.executorName", name]);
        test_box.git_ok(clone, &["config", "bay.producerName", "shared-checks"]);
    }
    let (_, first) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&first).arg("land"));
    assert_eq!(code, 0);
    test_box.bay_ok(&one, &["executor", "run", "--once"]);

    let (_, second) = test_box.start_and_edit(&repo);
    std::fs::write(second.join("src/other.rs"), "fn other() {}\n").unwrap();
    let (code, _) = result_json(test_box.bay(&second).arg("land"));
    assert_eq!(code, 0);
    let ran = test_box.bay_ok(&two, &["executor", "run", "--once"]);
    let result = &ran["detail"]["ran"][0];
    assert_eq!(result["decision"], "failed", "{ran}");
    assert!(
        result["detail"]
            .as_str()
            .unwrap()
            .contains("collides with another executor"),
        "{ran}"
    );
}

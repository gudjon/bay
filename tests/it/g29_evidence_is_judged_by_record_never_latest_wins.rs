//! G29. Evidence is judged by record, never latest-wins.
//!
//! The combine rules (an older pass with a newer `unknown` or `infra_failure` is pass; a pass and a
//! `product_failure` in either order is flaky; one environment does not satisfy another; a
//! commit-keyed failure is not charged to another commit; two records under one id are both
//! refused) are unit tests of `evidence.rs`; a result for a different commit is G3. Here: a
//! check that tries to read the producer key or write evidence from inside its sandbox
//! produces no counted evidence; one tree rechecked by another operation appends records
//! (reuse is off); one producer name used by two executors is reported as a collision. And
//! through `land`, with reuse off and one tree rechecked by a second change that makes the
//! same edit: a pass and a `product_failure` in either order is `CHECK_FLAKY`; an older pass
//! and a newer `unknown` lands; a commit-keyed failure on commit A does not block commit B
//! with the same tree; and a pass on one environment does not satisfy a second one.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

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
            &test_box.relay(&format!("echo run >> '{}'", runs.display())),
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

/// The adapter answer of one `product_failure` unit.
fn failing_result() -> String {
    passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"")
}

/// What two lands of one tree end in.
struct TwoRuns {
    first: (i32, serde_json::Value),
    second: (i32, serde_json::Value),
    /// Whether the second land moved trunk.
    moved: bool,
}

/// Two changes make the same edit and land one after the other, so the second candidate has
/// the first one's tree (on the trunk the first one made, or on the same trunk when the first
/// did not land). The one required check `tests` answers `first` (shell that prints a result,
/// or exits with none) on its first run and `second` on the next; `extra` is appended to its
/// `[[check]]` table. Reuse is off, so the second change runs the check again, and its verdict
/// is judged over every record on the tree.
fn two_runs_on_one_tree(first: &str, second: &str, extra: &str) -> TwoRuns {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let flag = test_box.path().join("ran-once");
    let before = format!(
        "if {ran}then {second}; else {first}; fi; exit 0",
        // Whether the check ran before, from the test: the check sees nothing of the box.
        ran = test_box.relay(&format!(
            "[ -e '{once}' ] && exit 0; : > '{once}'; exit 1",
            once = flag.display()
        ))
    );
    let files: Vec<(String, String)> = check_files(&before, "true", 1, 60)
        .into_iter()
        .map(|(path, text)| {
            if path == ".bay/policy.toml" {
                (path, format!("{text}{extra}"))
            } else {
                (path, text)
            }
        })
        .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (_, one) = test_box.start_and_edit(&repo);
    let (_, two) = test_box.start_and_edit(&repo);
    let first = result_json(test_box.bay(&one).args(["land", "--wait"]));
    let trunk = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let second = result_json(test_box.bay(&two).args(["land", "--wait"]));
    let moved = test_box.git_bare(&remote, &["rev-parse", "main"]) != trunk;
    TwoRuns {
        first,
        second,
        moved,
    }
}

#[test]
fn an_older_pass_and_a_newer_product_failure_on_one_tree_is_check_flaky_through_land() {
    let runs = two_runs_on_one_tree(&passing_result(1), &failing_result(), "");
    let (code, landed) = &runs.first;
    assert_eq!(*code, 0, "{landed}");
    let (code, flaky) = &runs.second;
    assert_eq!(*code, 10, "{flaky}");
    assert_eq!(flaky["reason"], "CHECK_FLAKY", "{flaky}");
    assert!(!runs.moved, "a flaky tree published: {flaky}");
}

#[test]
fn an_older_product_failure_and_a_newer_pass_on_one_tree_is_check_flaky_through_land() {
    let runs = two_runs_on_one_tree(&failing_result(), &passing_result(1), "");
    let (code, failed) = &runs.first;
    assert_eq!(*code, 10, "{failed}");
    assert_eq!(failed["reason"], "CHECK_FAILED", "{failed}");
    let (code, flaky) = &runs.second;
    assert_eq!(*code, 10, "{flaky}");
    assert_eq!(
        flaky["reason"], "CHECK_FLAKY",
        "the newer pass never wins: {flaky}"
    );
    assert!(!runs.moved, "a flaky tree published: {flaky}");
}

#[test]
fn an_older_pass_and_a_newer_unknown_on_one_tree_still_lands_through_land() {
    // The second run crashes with no result: `unknown`, which never cancels a pass.
    let runs = two_runs_on_one_tree(&passing_result(1), "exit 3", "");
    let (code, landed) = &runs.first;
    assert_eq!(*code, 0, "{landed}");
    let (code, again) = &runs.second;
    assert_eq!(*code, 0, "{again}");
    assert_eq!(again["decision"], "landed", "{again}");
}

#[test]
fn a_commit_keyed_failure_on_one_commit_does_not_block_another_with_the_same_tree() {
    // As the tree-keyed case above, which is `CHECK_FLAKY`; the check reads the commit, so the
    // failure on the first candidate is not charged to the second.
    let runs = two_runs_on_one_tree(
        &failing_result(),
        &passing_result(1),
        "inputs = [\"commit\"]\n",
    );
    let (code, failed) = &runs.first;
    assert_eq!(*code, 10, "{failed}");
    assert_eq!(failed["reason"], "CHECK_FAILED", "{failed}");
    let (code, landed) = &runs.second;
    assert_eq!(*code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    assert!(runs.moved, "trunk did not move: {landed}");
}

#[test]
fn a_pass_on_one_environment_does_not_satisfy_a_second_through_land() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    // `scripts/bay-check <environment>`: a pass on `local`, no result on `other`.
    let base = check_files("[ \"$1\" = other ] && exit 3", &passing_result(1), 1, 60);
    let script = base
        .iter()
        .find(|(path, _)| path == "scripts/bay-check")
        .unwrap()
        .1
        .clone();
    let mut policy = "revision = 1\n".to_owned();
    for environment in ["local", "other"] {
        use std::fmt::Write;
        let _ = write!(
            policy,
            "\n[[check]]\nid = \"tests\"\nenvironment = \"{environment}\"\ncommand = \"scripts/bay-check {environment}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n"
        );
    }
    let trunk = test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[
            ("scripts/bay-check".to_owned(), script),
            (".bay/policy.toml".to_owned(), policy),
        ],
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, blocked) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_UNKNOWN", "{blocked}");
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "main"]),
        trunk,
        "a pass on one environment published for two"
    );
}

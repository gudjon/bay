//! G213: a change whose merged tree equals trunk's tree ends `no_effect` (`DESIGN` § 6).
//!
//! Another change landed the same bytes first, or the change's own edits cancel out. Nothing
//! is published: no trunk commit, no landed marker, no check. The result is decision `ok`,
//! state `no_effect`, exit 0, with `detail.trunk_commit` naming the trunk commit whose tree it
//! is; the chain records `no_effect` as the operation's terminal state, and status shows it.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

fn evidence_on(test_box: &TestBox, repo: &std::path::Path, tree: &str) -> usize {
    let out = test_box
        .git(repo)
        .args([
            "notes",
            &format!("--ref=refs/notes/bay/evidence/{}", support::PRODUCER),
            "show",
            tree,
        ])
        .output()
        .unwrap();
    if !out.status.success() {
        return 0;
    }
    serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout)
        .unwrap()
        .len()
}

fn assert_no_effect(
    test_box: &TestBox,
    repo: &std::path::Path,
    id: &str,
    result: &serde_json::Value,
    trunk: &str,
) {
    assert_eq!(result["decision"], "ok", "{result}");
    assert_eq!(result["state"], "no_effect", "{result}");
    assert_eq!(result["detail"]["trunk_commit"], trunk, "{result}");
    assert_eq!(
        test_box
            .git_ok(repo, &["ls-remote", "origin", "refs/heads/main"])
            .split_whitespace()
            .next()
            .unwrap(),
        trunk,
        "trunk did not move"
    );
    let status = test_box.bay_ok(repo, &["status", id]);
    let change = &status["detail"]["changes"][0];
    assert_eq!(change["state"], "no_effect", "{change}");
    assert_eq!(change["operation"]["state"], "no_effect", "{change}");
    assert_eq!(change["operation"]["attempts"], 0, "no candidate: {change}");
    assert_eq!(change["landed"], serde_json::Value::Null, "{change}");
}

#[test]
fn a_change_whose_bytes_another_landed_first_ends_no_effect() {
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
    // Both write the same src/new.rs.
    let (_, first) = test_box.start_and_edit(&repo);
    let (second_id, second) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&first).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let trunk = landed["detail"]["commit"].as_str().unwrap().to_owned();
    let tree = test_box.git_ok(&repo, &["rev-parse", &format!("{trunk}^{{tree}}")]);
    let records = evidence_on(&test_box, &repo, &tree);

    let (code, result) = result_json(test_box.bay(&second).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{result}");
    assert_no_effect(&test_box, &repo, &second_id, &result, &trunk);
    assert_eq!(
        evidence_on(&test_box, &repo, &tree),
        records,
        "no check ran for a change with no effect"
    );
}

#[test]
fn a_change_whose_edits_cancel_out_ends_no_effect() {
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
    let trunk = test_box
        .git_ok(&repo, &["ls-remote", "origin", "refs/heads/main"])
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let (id, view) = test_box.start_and_edit(&repo);
    std::fs::remove_file(view.join("src/new.rs")).unwrap();
    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{result}");
    assert_no_effect(&test_box, &repo, &id, &result, &trunk);
}

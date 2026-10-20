//! G32. A change lands at most once, and a different revision is never reported as landed.
//!
//! Land revision r1; queue r2 of the same change before r1 publishes: r1's operation is
//! superseded and r2 publishes; queue r3 after r2 landed: refused `CHANGE_ALREADY_LANDED`,
//! never "landed"; an unsigned commit that quotes the `Change-Id` trailer is an anomaly, not
//! a landing.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_newer_revision_supersedes_and_a_later_one_is_refused() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    let (id, view) = test_box.start_and_edit(&repo);

    let (code, r1) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{r1}");
    std::fs::write(view.join("src/new.rs"), "fn new() { r2 }\n").unwrap();
    let (code, r2) = result_json(test_box.bay(&view).arg("land"));
    assert_eq!(code, 0, "{r2}");
    assert_eq!(r2["op_id"], format!("{id}.2"));

    let executor = test_box.clone_of(&remote, "executor");
    test_box.use_keys(&executor, &keys);
    let ran = test_box.bay_ok(&executor, &["executor", "run", "--once"]);
    let results = ran["detail"]["ran"].as_array().unwrap();
    assert_eq!(results.len(), 1, "only r2 runs: {ran}");
    assert_eq!(results[0]["op_id"], format!("{id}.2"));
    assert_eq!(results[0]["decision"], "landed");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/new.rs"]),
        "fn new() { r2 }"
    );
    let record = test_box.git_bare(
        &remote,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    assert_eq!(record["operations"][0]["state"], "superseded");
    assert_eq!(
        record["operations"][0]["outcome"],
        format!("superseded by {id}.2")
    );

    // r3 after r2 landed: refused, never reported as landed.
    std::fs::write(view.join("src/new.rs"), "fn new() { r3 }\n").unwrap();
    let (code, r3) = result_json(test_box.bay(&view).args(["land", "--wait"]));
    assert_eq!(code, 10, "{r3}");
    assert_eq!(r3["reason"], "CHANGE_ALREADY_LANDED");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

#[test]
fn an_unsigned_commit_quoting_the_change_id_is_an_anomaly() {
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
    let (id, _) = test_box.start_and_edit(&repo);
    test_box.git_ok(&repo, &["fetch", "-q", "origin"]);
    test_box.git_ok(&repo, &["reset", "-q", "--hard", "origin/main"]);
    test_box.git_ok(
        &repo,
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!("copied\n\nChange-Id: {id}\nRevision-Id: x\n"),
        ],
    );
    test_box.git_ok(&repo, &["push", "-q", "origin", "HEAD:main"]);
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    assert_ne!(change["state"], "landed", "{status}");
    assert_eq!(change["anomalies"].as_array().unwrap().len(), 1);
}

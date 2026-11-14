//! BACKLOG 238 (a fleet finding, 2026-11-14). A landed change merged again, on a main read
//! before its landing reached it, was submitted again: 3xq4d7ow landed e403611 on 2026-11-12,
//! and its next `bay merge` read main at 02e12cc, found no publication there, and wrote
//! operation .2 and the change `ready`. No runner answers that operation (a merged operation
//! ends the change), so its record said "waits its turn in the queue" for days and audits
//! counted it live. The record decides, as for another revision (G682): a merged operation of
//! this revision answers `merged` with its landing, whatever main the process read, and
//! writes nothing.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use support::{TestBox, check_files, passing_result, result_json};

fn record(test_box: &TestBox, repo: &Path, id: &str) -> serde_json::Value {
    let text = test_box.git_ok(
        repo,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    );
    serde_json::from_str(&text).unwrap()
}

#[test]
fn a_landed_change_merged_again_on_a_main_read_before_its_landing_is_merged() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let before = test_box.git_ok(&repo, &["rev-parse", "refs/remotes/origin/main"]);
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    let landing = test_box.git_bare(&remote, &["rev-parse", "main"]);
    assert_eq!(record(&test_box, &repo, &id)["state"], "landed");

    // This box's main is the one it read before the landing, and the remote cannot be asked.
    test_box.git_ok(&repo, &["update-ref", "refs/remotes/origin/main", &before]);
    let away = remote.with_extension("away");
    std::fs::rename(&remote, &away).unwrap();
    let (code, again) = result_json(test_box.bay(&view).arg("merge"));
    std::fs::rename(&away, &remote).unwrap();

    assert_eq!(code, 0, "{again}");
    assert_eq!(again["decision"], "merged", "{again}");
    assert_eq!(again["detail"]["commit"], landing.as_str(), "{again}");
    let after = record(&test_box, &repo, &id);
    assert_eq!(after["state"], "landed", "{after}");
    assert_eq!(
        after["operations"].as_array().unwrap().len(),
        1,
        "no operation is written: {after}"
    );
    // Back online, nothing runs and nothing lands again.
    let executor = test_box.clone_of(&remote, "executor");
    let ran = test_box.bay_ok(&executor, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"], serde_json::json!([]), "{ran}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

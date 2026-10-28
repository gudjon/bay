//! G426. Chains a pass's glob fetch refuses cost one more fetch together, never one each.
//!
//! A pass reads every change chain in one fetch. Git refuses a chain this store holds apart
//! from the remote's, and bay read each refused chain again on its own, one or two fetches
//! each. Measured 2026-10-28, a runner kept eight such chains pass after pass, and each single
//! fetch cost about fifty seconds on a loaded box: ten silent minutes a pass. Now the refused
//! chains are read in one fetch, so a pass with six such chains fetches as often as one with
//! one, read from the trace a test build writes when `BAY_TEST_GIT_TRACE` names a file (G171).
//!
//! The guard to mutate: the `remote_chains` call in `Repo::fetch_chains` (`src/repo.rs`); read
//! each differing chain with `remote_chain` and the fetches grow with the chains.

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

/// A store whose executor clone holds `apart` of six change chains apart from the remote's.
fn store(apart: usize) -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let mut ids = Vec::new();
    for _ in 0..6 {
        let (id, view) = test_box.start_and_edit(&repo);
        let (code, result) = result_json(test_box.bay(&view).args(["abandon"]));
        assert_eq!(code, 0, "{result}");
        ids.push(id);
    }
    let executor = test_box.clone_of(&remote, "executor");
    // A first pass brings every chain here.
    let _ = fetches(&test_box, &executor, "first");
    for id in ids.iter().take(apart) {
        let name = format!("refs/bay/changes/{id}");
        let tree = test_box.git_ok(&executor, &["rev-parse", &format!("{name}^{{tree}}")]);
        let alone = test_box.git_ok(
            &executor,
            &["commit-tree", tree.trim(), "-m", "apart from the remote"],
        );
        test_box.git_ok(&executor, &["update-ref", &name, alone.trim()]);
    }
    (test_box, executor)
}

/// The `fetch` spawns of one `bay runner run --once` in `dir`, from bay's own process only.
fn fetches(test_box: &TestBox, dir: &Path, name: &str) -> usize {
    let trace = test_box.path().join(format!("{name}-trace.jsonl"));
    let (code, result) = result_json(
        test_box
            .bay(dir)
            .args(["runner", "run", "--once"])
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{result}");
    let text = std::fs::read_to_string(&trace).expect("the trace was written");
    let lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}")))
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref() && line["sub"] == "fetch")
        .count()
}

#[test]
fn six_chains_apart_from_the_remote_cost_no_more_fetches_than_one() {
    let (one_box, one) = store(1);
    let (six_box, six) = store(6);
    let with_one = fetches(&one_box, &one, "one");
    let with_six = fetches(&six_box, &six, "six");
    assert!(
        with_six <= with_one,
        "six refused chains cost {with_six} fetches, one cost {with_one}"
    );
}

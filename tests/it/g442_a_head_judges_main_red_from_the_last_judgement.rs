//! G442. A head judges main's red from the last judgement, reading only what moved since.
//!
//! The head step `red` was a head's largest: p50 78 s, p90 157 s over 18 heads (2026-10-28, all
//! four runners of a fleet). Its cache (G430) answered only for the same main tip, registry and
//! evidence, and on a busy line main or a note moves before almost every head, so each head
//! judged the whole `[red] window` again. Now the judgement starts from the last one
//! (`red::judge`, G195): it reads the landings after that tip and the notes that moved
//! (BACKLOG 97).
//!
//! The guard to mutate: `red::red` passes its last judgement to `judge` (`src/red.rs`; pass
//! `None` and every head judges in full).

// Each case runs a shell adapter, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// A post check `suite` on every path, which no runner here answers.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\n";

/// Starts change `name` on `src/`, writes `src/<name>.rs` in its view and submits it.
fn queue(test_box: &TestBox, repo: &Path, name: &str) {
    let started = test_box.bay_ok(repo, &["start", name, "--paths", "src"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(format!("src/{name}.rs")), "fn x() {}\n").unwrap();
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
}

#[test]
fn the_next_head_judges_from_the_last_judgement_not_in_full() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
        }
    }
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    // Two heads in one pass: the first lands, so main moves before the second's red step.
    queue(&test_box, &repo, "one");
    queue(&test_box, &repo, "two");
    let trace = test_box.path().join("red-trace");
    let (_, ran) = result_json(
        test_box
            .bay(&repo)
            .args(["runner", "run", "--once"])
            .env("BAY_TEST_RED_TRACE", &trace),
    );
    let merged = ran["detail"]["ran"].as_array().map_or(0, |ran| {
        ran.iter().filter(|op| op["decision"] == "merged").count()
    });
    assert_eq!(merged, 2, "both changes merged in the pass: {ran}");
    let judged = std::fs::read_to_string(&trace).unwrap_or_default();
    let lines: Vec<&str> = judged.lines().collect();
    assert_eq!(lines.len(), 2, "one judgement per main tip: {judged}");
    assert!(lines[0].ends_with(" in full"), "{judged}");
    let first = lines[0].split_whitespace().next().unwrap();
    assert!(
        lines[1].ends_with(&format!(" from {first}")),
        "the second head judged in full: {judged}"
    );
}

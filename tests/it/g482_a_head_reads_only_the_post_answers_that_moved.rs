//! G482 (bay 0.6.0, lever S4; a fleet measurement, 2026-10-28, v12-landingbay E29). A head
//! judges main's red reading only the landings whose post answers can have changed.
//!
//! A merging head's `red` step took 69-185 s, median 76 s, on 8 of 14 merges (bay 0.5.40-0.5.42,
//! one runner), about half of its time to main. The judgement started from the last one (G442),
//! but it still read every landing back to a red check's last pass, and the whole window again
//! when a deciding landing stopped deciding: one read of every evidence ref per landing. Now a
//! landing's answers, which change only when the notes of its tree move, are kept from the last
//! judgement, and only the new landings and those whose notes moved are read. Main is still
//! judged from this box's own copy of the records (BACKLOG 113).
//!
//! The guards to mutate, in `red::judge_from` (`src/red.rs`): carry no answers and the second
//! head reads the red check's whole suspect range again; carry the answers of a landing whose
//! notes moved and a judgement in this process misses the red a new record shows.

// Each case runs a shell adapter, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// Three post checks on `src/**`; `suite` fails on any tree holding `src/bad.rs`.
fn post(id: &str) -> String {
    format!(
        "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check {id}\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n"
    )
}

/// Lands a change that writes `path` (under `src/` or `docs/`), with `bay merge --wait`.
fn land(test_box: &TestBox, repo: &Path, path: &str) {
    let dir = path.split('/').next().unwrap();
    let started = test_box.bay_ok(repo, &["start", path, "--paths", &format!("{dir}/")]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
}

/// A trunk with `src/` and `docs/`, a pre check every change passes and three post checks on
/// `src/**`, of which `suite` fails on any tree holding `src/bad.rs`; runners run post checks.
fn trunk() -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let mut files = check_files(
        &format!("[ \"$1\" = suite ] && [ -e src/bad.rs ] && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            for id in ["suite", "lint", "style"] {
                text.push_str(&post(id));
            }
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\n".to_owned(),
    ));
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("docs/readme.md", "readme\n")],
        &files,
    );
    (test_box, repo)
}

/// Runner passes in `repo` until `done` holds of `bay status`, at most `passes`.
fn passes_until(test_box: &TestBox, repo: &Path, passes: usize, done: impl Fn(&Value) -> bool) {
    for _ in 0..passes {
        let (code, ran) = result_json(test_box.bay(repo).args(["runner", "run", "--once"]));
        assert_eq!(code, 0, "{ran}");
        if done(&test_box.bay_ok(repo, &["status"])) {
            return;
        }
    }
    panic!(
        "not reached in {passes} passes: {}",
        test_box.bay_ok(repo, &["status"])
    );
}

/// Every post check passes on a landing of `src/good.rs`.
fn all_pass(test_box: &TestBox, repo: &Path) {
    land(test_box, repo, "src/good.rs");
    passes_until(test_box, repo, 6, |status| {
        status["detail"]["red_pending"]
            .as_array()
            .is_some_and(Vec::is_empty)
    });
}

#[test]
fn the_next_head_reads_only_the_landings_whose_answers_moved() {
    let (test_box, repo) = trunk();
    // Every post check passes on one landing, then eight landings later `suite` fails on main's
    // tip: its suspect range reads back over the eight to that pass.
    all_pass(&test_box, &repo);
    land(&test_box, &repo, "src/bad.rs");
    for n in 0..7 {
        land(&test_box, &repo, &format!("src/more{n}.rs"));
    }
    passes_until(&test_box, &repo, 6, |status| {
        status["detail"]["red"]
            .as_array()
            .is_some_and(|red| !red.is_empty())
    });
    // Two docs changes, which the red does not hold, merge in one pass: the second head judges
    // main from the first one's judgement, one landing later.
    for name in ["d1", "d2"] {
        let started = test_box.bay_ok(&repo, &["start", name, "--paths", "docs/"]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(format!("docs/{name}.md")), "doc\n").unwrap();
        let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
        assert_eq!(code, 0, "{queued}");
    }
    let trace = test_box.path().join("red-trace");
    let (code, ran) = result_json(
        test_box
            .bay(&repo)
            .args(["runner", "run", "--once"])
            .env("BAY_TEST_RED_TRACE", &trace),
    );
    assert_eq!(code, 0, "{ran}");
    let merged = ran["detail"]["ran"].as_array().map_or(0, |ran| {
        ran.iter().filter(|op| op["decision"] == "merged").count()
    });
    assert_eq!(merged, 2, "both docs changes merged in the pass: {ran}");
    let judged = std::fs::read_to_string(&trace).unwrap_or_default();
    let read = |line: &str| -> usize {
        line.split_whitespace()
            .skip_while(|word| *word != "read")
            .nth(1)
            .and_then(|count| count.parse().ok())
            .unwrap_or_else(|| panic!("a judgement names no landings read: {judged}"))
    };
    let lines: Vec<&str> = judged.lines().collect();
    assert!(lines.len() >= 2, "{judged}");
    let (first, second) = (read(lines[0]), read(lines[1]));
    assert!(
        first >= 9,
        "the first judgement did not read the suspect range: {judged}"
    );
    assert!(
        second <= 2,
        "the second head read {second} landings, the first {first}: {judged}"
    );
    eprintln!("G482: the first head read {first} landings, the next one {second}");
}

#[test]
fn a_landing_whose_notes_moved_is_read_again() {
    let (test_box, repo) = trunk();
    all_pass(&test_box, &repo);
    land(&test_box, &repo, "src/bad.rs");
    // This process judges main's tip before any post check answers on it: green.
    let git = bay::git::Git::new(&repo);
    test_box.git_ok(&repo, &["fetch", "-q", "origin", "main"]);
    let tip = test_box.git_ok(&repo, &["rev-parse", "refs/remotes/origin/main"]);
    assert_eq!(
        bay::red::red(&git, &tip, None).unwrap(),
        Vec::new(),
        "main was red before any post check answered on its tip"
    );
    // A runner answers `suite` on that tip, red, into this store's notes.
    passes_until(&test_box, &repo, 6, |status| {
        status["detail"]["red"]
            .as_array()
            .is_some_and(|red| !red.is_empty())
    });
    let red = bay::red::red(&git, &tip, None).unwrap();
    assert!(
        !red.is_empty(),
        "a judgement read the tip's answers from before its notes moved"
    );
}

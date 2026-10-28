//! G428. A head on a red main's paths waits before its input providers run.
//!
//! A change that touches a red main's paths waits for the repair whatever its inputs say, and
//! a batch already asks the red gate before it checks anything. A head with no batch ran every
//! input provider first. Measured 2026-10-28, its providers ran twelve minutes while it held
//! the head, and then it waited on the red main. Now the red gate comes first.
//!
//! G429, the same rule for a conflict: a head that does not merge onto main is refused before
//! its providers run (measured the same night: four to eight minutes of providers, then
//! `MERGE_CONFLICT`).
//!
//! G430: main's red is judged once per tip in a runner, never once per head; with a red
//! window of 250 each judgement took five to twelve minutes on a loaded box.
//!
//! The guards to mutate: the order in `build_and_check` (`src/publish.rs`); run `input_gate`
//! before `red_gate`, or before the merge, and the provider runs for a head that cannot land.
//! And the cached judgement in `red::red` (`src/red.rs`); judge every call afresh and one pass
//! judges the same tip once per head.

// Each case runs shell scripts (adapters, a provider), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json, write_executable};

const INPUT: &str = "contract:api";

const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// A trunk whose post check fails on any tree holding `src/bad.rs`, with a provider for
/// `contract:api` that takes ten seconds, so a head that ran it reports ten seconds or more
/// in the runner's journal line. Returns the box and the work repository.
fn trunk() -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    write_executable(
        &repo.join("scripts/api-version"),
        "#!/bin/sh\ncat >/dev/null\nsleep 10\nprintf '{\"version\":\"v1\",\"consumers\":[{\"path\":\"src/\",\"kind\":\"executable\"}]}'\n",
    );
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    let mut files = check_files(
        &format!("[ \"$1\" = suite ] && [ -e src/bad.rs ] && {{ {failing}; exit 0; }}"),
        &passing_result(1),
        1,
        60,
    );
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(POST);
            let _ = write!(
                text,
                "\n[[input]]\nname = \"{INPUT}\"\nprovider = \"scripts/api-version\"\nadapter = [\"scripts/api-version\"]\npath = [\"/usr/bin\", \"/bin\"]\n"
            );
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    (test_box, repo)
}

/// One runner pass in `repo`: its result and its journal lines.
fn pass(test_box: &TestBox, repo: &Path) -> (serde_json::Value, String) {
    let out = test_box
        .bay(repo)
        .args(["--json", "runner", "run", "--once"])
        .output()
        .unwrap();
    let result = serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
    (result, String::from_utf8_lossy(&out.stderr).into_owned())
}

/// Whether `change`'s journal line reports ten seconds or more: its providers ran.
fn providers_ran(journal: &str, change: &str) -> bool {
    journal
        .lines()
        .filter(|line| line.starts_with("bay runner:") && line.contains(change))
        .filter_map(|line| {
            let (_, tail) = line.rsplit_once('(')?;
            tail.split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse::<u64>()
                .ok()
        })
        .any(|seconds| seconds >= 10)
}

/// Lands a change that writes `path`.
fn land(test_box: &TestBox, repo: &Path, path: &str) {
    let started = test_box.bay_ok(repo, &["start", path, "--paths", "src"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
}

#[test]
fn a_head_on_a_red_mains_paths_waits_before_its_providers_run() {
    let (test_box, repo) = trunk();
    // A landing breaks the post check, and a runner pass finds main red.
    land(&test_box, &repo, "src/bad.rs");
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    assert!(ran.to_string().contains("product_failure"), "{ran}");
    // A change on the red paths that declares the input, submitted.
    let started = test_box.bay_ok(
        &repo,
        &["start", "next", "--input", INPUT, "--paths", "src"],
    );
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/next.rs"), "fn next() {}\n").unwrap();
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let id = queued["change"].as_str().unwrap().to_owned();
    let (held, journal) = pass(&test_box, &repo);
    assert!(held.to_string().contains("MAIN_RED"), "{held}");
    assert!(
        !providers_ran(&journal, &id),
        "the provider ran for a head that waits on a red main: {journal}"
    );
}

#[test]
fn a_head_that_conflicts_with_main_is_refused_before_its_providers_run() {
    let (test_box, repo) = trunk();
    // The head declares the input and edits a line that another change edits first.
    let started = test_box.bay_ok(
        &repo,
        &["start", "mine", "--input", INPUT, "--paths", "src"],
    );
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/a.rs"), "fn a() { 1; }\n").unwrap();
    let theirs = test_box.bay_ok(&repo, &["start", "theirs", "--paths", "src"]);
    let their_view = PathBuf::from(theirs["detail"]["view"].as_str().unwrap());
    std::fs::write(their_view.join("src/a.rs"), "fn a() { 2; }\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&their_view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{queued}");
    let id = queued["change"].as_str().unwrap().to_owned();
    let (refused, journal) = pass(&test_box, &repo);
    assert!(refused.to_string().contains("MERGE_CONFLICT"), "{refused}");
    assert!(
        !providers_ran(&journal, &id),
        "the provider ran for a head that conflicts with main: {journal}"
    );
}

#[test]
fn a_red_main_is_judged_once_for_every_head_on_its_tip() {
    let (test_box, repo) = trunk();
    land(&test_box, &repo, "src/bad.rs");
    let ran = test_box.bay_ok(&repo, &["runner", "run", "--once"]);
    assert!(ran.to_string().contains("product_failure"), "{ran}");
    // Three changes on the red paths, each the head in turn in one pass.
    for name in ["one", "two", "three"] {
        let started = test_box.bay_ok(&repo, &["start", name, "--paths", "src"]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(format!("src/{name}.rs")), "fn x() {}\n").unwrap();
        let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
        assert_eq!(code, 0, "{queued}");
    }
    let trace = test_box.path().join("red-trace");
    let (_, held) = result_json(
        test_box
            .bay(&repo)
            .args(["runner", "run", "--once"])
            .env("BAY_TEST_RED_TRACE", &trace),
    );
    let waited = held.to_string().matches("MAIN_RED").count();
    assert!(
        waited >= 3,
        "three heads did not wait on the red main: {held}"
    );
    let judged = std::fs::read_to_string(&trace).unwrap_or_default();
    assert_eq!(
        judged.lines().count(),
        1,
        "main's red was judged once per head on one tip: {judged}"
    );
}

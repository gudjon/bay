//! G606 (a fleet finding, 2026-10-31: each `bay status` on a busy repository judged main's red
//! over the whole window again, about 9 s of a 42 s status, though a status a minute before had
//! judged the same landings). A status saves its red judgement, what decided it and the answers
//! it read, in the repository's common directory; the next status starts from it and reads only
//! what moved since. One that another bay saved is never read. Its answer is the one it gave
//! before.
//!
//! Guards to mutate, in `src/red.rs`: `shown` judges without the saved judgement, and the second
//! status reads the window again; `saved` drops its version filter, and a status reads answers
//! another bay saved.

// Each case runs a shell adapter, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// A post check on `src/**` that fails on any tree holding `src/bad.rs`.
const POST: &str = "\n[[check]]\nid = \"suite\"\nenvironment = \"local\"\nstage = \"post\"\ncommand = \"scripts/bay-check suite\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 60\nwhen_paths = [\"src/**\"]\n";

/// Lands a change that writes `path` under `src/`, with `bay merge --wait`.
pub(crate) fn land(test_box: &TestBox, repo: &Path, path: &str) {
    let started = test_box.bay_ok(repo, &["start", path, "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
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

/// One `bay status` with the red trace on: its red rows and the trace lines it wrote.
fn status(test_box: &TestBox, repo: &Path) -> (Value, Vec<String>) {
    let trace = test_box.path().join("red-trace");
    // The sweep of an earlier read writes its judgement and its trace too.
    support::sweeps_ended(repo);
    let _ = std::fs::remove_file(&trace);
    let (code, status) = result_json(
        test_box
            .bay(repo)
            .arg("status")
            .env("BAY_TEST_RED_TRACE", &trace)
            // What the read judges is under test, never a saved answer of it (G714).
            .env("BAY_TEST_SAVED", "off"),
    );
    assert_eq!(code, 0, "{status}");
    support::sweeps_ended(repo);
    let lines = std::fs::read_to_string(&trace)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    (status["detail"]["red"].clone(), lines)
}

#[test]
fn a_status_judges_red_from_the_judgement_the_last_status_saved() {
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
            text.push_str(POST);
        }
    }
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\npost = true\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);

    // `suite` passes on one landing, then fails three landings later: main is red.
    land(&test_box, &repo, "src/good.rs");
    land(&test_box, &repo, "src/bad.rs");
    for n in 0..2 {
        land(&test_box, &repo, &format!("src/more{n}.rs"));
    }
    passes_until(&test_box, &repo, 8, |status| {
        status["detail"]["red"]
            .as_array()
            .is_some_and(|red| !red.is_empty())
    });

    // The statuses above saved their judgements: a status with none saved judges the window.
    let saved = repo.join(".git/bay/red-judged.json");
    support::sweeps_ended(&repo);
    assert!(saved.is_file(), "no status saved a judgement");
    std::fs::remove_file(&saved).unwrap();
    let (first, judged) = status(&test_box, &repo);
    assert!(
        judged.len() == 1 && judged[0].ends_with("in full"),
        "the first status did not judge the window: {judged:?}"
    );
    assert!(saved.is_file(), "the status saved no judgement");

    // Nothing moved: the next status starts from the saved judgement, reads no answers, and
    // finds the same red.
    let (second, judged) = status(&test_box, &repo);
    assert!(
        judged.len() == 1 && judged[0].contains(" read 0 from "),
        "the next status judged again what nothing moved: {judged:?}"
    );
    assert_eq!(second, first, "the saved judgement changed the red");

    // A judgement another bay saved is never read: that status judges the window again.
    let text = std::fs::read_to_string(&saved).unwrap();
    let mut other: Value = serde_json::from_str(&text).unwrap();
    other["bay"] = Value::from("0.0.1");
    std::fs::write(&saved, other.to_string()).unwrap();
    let (third, judged) = status(&test_box, &repo);
    assert!(
        judged.len() == 1 && judged[0].ends_with("in full"),
        "a status read the judgement another bay saved: {judged:?}"
    );
    assert_eq!(third, first, "{third}");
}

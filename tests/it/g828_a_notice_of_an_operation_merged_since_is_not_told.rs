//! G828 (a fleet finding, 2026-11-19: a session was told to retry an operation one runner
//! refused `MERGE_OUTCOME_UNKNOWN`, 15 min after another runner merged that same operation): a
//! refusal or a failed check of an operation its change has merged since, itself or a later
//! operation, asks for nothing, so a session is not told it.
//!
//! Guard to mutate: `src/notice.rs` `tell`: drop the `merged(event)` test (the holder is told
//! of a failed check its change has since merged past).

use crate::support;

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use serde_json::Value;
use support::TestBox;

/// A recorded Claude Code payload `name` for `session` in `cwd`.
fn fixture(name: &str, session: &str, cwd: &Path) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302")
        .join(name);
    let mut payload: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    payload["session_id"] = Value::from(session);
    payload["cwd"] = Value::from(cwd.display().to_string());
    payload
}

/// `bay hook <event>` of `payload` from `cwd`: its stdout.
fn hook(test_box: &TestBox, cwd: &Path, event: &str, payload: &Value) -> String {
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// Submits the change in `view` as session `s-holder` and lets the runner in `runner` take
/// it once: the runner's result for the operation.
fn submit(test_box: &TestBox, view: &Path, runner: &Path) -> Value {
    let (code, submitted) = support::result_json(
        test_box
            .bay(view)
            .env("BAY_SESSION", "s-holder")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{submitted}");
    let ran = test_box.bay_ok(runner, &["runner", "run", "--once"]);
    ran["detail"]["ran"][0].clone()
}

#[test]
fn a_failed_check_of_an_operation_its_change_merged_past_is_not_told() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let failing = support::passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &support::check_files(
            &format!("grep -q bad src/a.rs && {{ {failing}; exit 0; }}"),
            &support::passing_result(1),
            1,
            60,
        ),
    );
    let runner = test_box.clone_of(&test_box.path().join("repo-remote.git"), "runner");
    hook(
        &test_box,
        &repo,
        "session.start",
        &fixture("01-SessionStart.json", "s-holder", &repo),
    );

    // GIVEN a change of the session whose first operation failed its check
    let started = support::result_json(test_box.bay(&repo).env("BAY_SESSION", "s-holder").args([
        "start",
        "a failing edit",
        "--paths",
        "src/",
    ]))
    .1;
    let id = started["change"].as_str().unwrap().to_owned();
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/a.rs"), "fn a() { bad }\n").unwrap();
    let first = submit(&test_box, &view, &runner);
    assert_eq!(first["reason"], "CHECK_FAILED", "{first}");

    // AND its next operation merged before the session read the log
    std::fs::write(view.join("src/a.rs"), "fn a() { 1; }\n").unwrap();
    let second = submit(&test_box, &view, &runner);
    assert_eq!(second["decision"], "merged", "{second}");
    test_box.bay_ok(&repo, &["status"]);

    // WHEN the session's next prompt reads the log
    let told = hook(
        &test_box,
        &repo,
        "turn.start",
        &fixture("02-UserPromptSubmit.json", "s-holder", &repo),
    );

    // THEN the failed check is not told: nothing is left to retry
    assert!(
        !told.contains(&format!("change {id}:")),
        "the session was told of a failed check its change merged past: {told}"
    );
}

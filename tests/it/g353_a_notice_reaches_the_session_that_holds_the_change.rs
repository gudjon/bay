//! G353 (owner ruling 2026-10-24, BACKLOG 64 F2): a notice reaches the session that holds the
//! change.
//!
//! A refusal of a change is told to its actor's sessions (`INTERFACES` § 2). The actor defaults
//! to the machine's user, so every agent on one machine heard every other agent's refusals, and
//! a fleet's alerts went to the wrong agent. Now, while the session that holds the change (the
//! one its chain names) is live on this machine, only that session is told; once it is not, any
//! session of the actor is, so no refusal goes untold.
//!
//! Guard to mutate: `src/notice.rs` `deliver`: let `owns` match on the actor alone (another
//! agent's session is told of the holder's refusal).

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

/// The sessions' start, then a change `s-holder` opened and submitted whose check fails, run by
/// a runner in another clone; `bay status` brings the runner's log. `holder_started` says
/// whether the holder's session ever ran a hook here (so is live). Returns the box, the main
/// checkout and the change.
fn refused(holder_started: bool) -> (TestBox, std::path::PathBuf, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let failing = support::passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &support::check_files("true", &failing, 1, 60),
    );
    let runner = test_box.clone_of(&test_box.path().join("repo-remote.git"), "runner");
    hook(
        &test_box,
        &repo,
        "session.start",
        &fixture("01-SessionStart.json", "s-other", &repo),
    );
    if holder_started {
        hook(
            &test_box,
            &repo,
            "session.start",
            &fixture("01-SessionStart.json", "s-holder", &repo),
        );
    }
    let started = support::result_json(test_box.bay(&repo).env("BAY_SESSION", "s-holder").args([
        "start",
        "a failing edit",
        "--paths",
        "src/",
    ]))
    .1;
    let id = started["change"].as_str().unwrap().to_owned();
    let view = std::path::PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/a.rs"), "fn a() { 1; }\n").unwrap();
    let (code, submitted) = support::result_json(
        test_box
            .bay(&view)
            .env("BAY_SESSION", "s-holder")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{submitted}");
    let ran = test_box.bay_ok(&runner, &["runner", "run", "--once"]);
    assert_eq!(ran["detail"]["ran"][0]["reason"], "CHECK_FAILED", "{ran}");
    test_box.bay_ok(&repo, &["status"]);
    (test_box, repo, id)
}

/// What a prompt of `session` in `repo` is told.
fn told(test_box: &TestBox, repo: &Path, session: &str) -> String {
    hook(
        test_box,
        repo,
        "turn.start",
        &fixture("02-UserPromptSubmit.json", session, repo),
    )
}

#[test]
fn while_the_holder_is_live_only_the_holder_is_told() {
    let (test_box, repo, id) = refused(true);
    let other = told(&test_box, &repo, "s-other");
    assert!(
        !other.contains(&format!("change {id}")),
        "another agent heard the holder's refusal: {other}"
    );
    let holder = told(&test_box, &repo, "s-holder");
    assert!(
        holder.contains(&format!("change {id}: check_failed CHECK_FAILED")),
        "{holder}"
    );
}

#[test]
fn a_holder_that_is_not_live_leaves_the_notice_to_any_session_of_the_actor() {
    let (test_box, repo, id) = refused(false);
    let other = told(&test_box, &repo, "s-other");
    assert!(
        other.contains(&format!("change {id}: check_failed CHECK_FAILED")),
        "the refusal went untold: {other}"
    );
}

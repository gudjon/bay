//! G166. `bay status --fleet` shows, per session, how far its view's trunk is behind trunk, and
//! the session's age (`plans/wave2.md` G166; `DESIGN` § 11a, `INTERFACES` § 5a).
//!
//! A session's entry counts the trunk commits its change's base lacks (`behind_commits`), from
//! local refs only: the trunk tracking ref the last fetch left and the change chain this cache
//! holds. Its age (`session_age_s`) runs from the first event the hook recorded for the session
//! (`started`), not from its latest one (`age_s`).

// Each case runs the hook as a process with a payload on stdin, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use support::TestBox;

fn fixture(name: &str, session: &str, cwd: &Path) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.302")
        .join(name);
    let mut payload: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    payload["session_id"] = Value::from(session);
    payload["cwd"] = Value::from(cwd.display().to_string());
    payload
}

fn hook(test_box: &TestBox, view: &Path, event: &str, name: &str, session: &str) {
    let payload = fixture(name, session, view);
    let mut child = test_box
        .bay_process(view)
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
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{event}: {output:?}");
}

/// Commits `file` on trunk from a second clone and pushes it.
fn trunk_commit(test_box: &TestBox, other: &Path, file: &str) {
    test_box.git_ok(other, &["pull", "-q", "origin", "main"]);
    std::fs::write(other.join(file), "x\n").unwrap();
    test_box.git_ok(other, &["add", "--all"]);
    test_box.git_ok(other, &["commit", "-q", "-m", file]);
    test_box.git_ok(other, &["push", "-q", "origin", "HEAD:main"]);
}

#[test]
fn a_sessions_entry_counts_its_trunk_lag_and_its_age() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    test_box.git_ok(&repo, &["config", "bay.machine", "box-a"]);
    let (id, view) = test_box.start_and_edit(&repo);
    hook(
        &test_box,
        &view,
        "session-start",
        "01-SessionStart.json",
        "s-lag",
    );

    let other = test_box.clone_of(&test_box.path().join("repo-remote.git"), "other");
    trunk_commit(&test_box, &other, "one.txt");
    trunk_commit(&test_box, &other, "two.txt");
    std::thread::sleep(Duration::from_millis(2100));
    hook(
        &test_box,
        &view,
        "post-tool",
        "08-PostToolBatch.json",
        "s-lag",
    );
    // `bay status` is the fetch; the map reads what it left.
    test_box.bay_ok(&repo, &["status"]);

    let (code, result) = support::result_json(test_box.bay(&repo).args(["status", "--fleet"]));
    assert_eq!(code, 0, "{result}");
    let row = result["detail"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["session"] == "s-lag")
        .unwrap_or_else(|| panic!("{result}"));
    assert_eq!(row["change"], id.as_str(), "{row}");
    assert_eq!(row["behind"], true, "{row}");
    assert_eq!(row["behind_commits"], 2, "{row}");
    let age = row["age_s"].as_u64().unwrap();
    let session_age = row["session_age_s"]
        .as_u64()
        .unwrap_or_else(|| panic!("{row}"));
    assert!(session_age >= 2 && session_age >= age, "{row}");
    assert!(row["started"].is_string(), "{row}");

    // A view on trunk's tip is behind by nothing.
    let (fresh, fresh_view) = test_box.start_and_edit(&repo);
    hook(
        &test_box,
        &fresh_view,
        "session-start",
        "01-SessionStart.json",
        "s-tip",
    );
    let (_, result) = support::result_json(test_box.bay(&repo).args(["status", "--fleet"]));
    let row = result["detail"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["session"] == "s-tip")
        .unwrap_or_else(|| panic!("{result}"));
    assert_eq!(row["change"], fresh.as_str(), "{row}");
    assert_eq!(row["behind_commits"], 0, "{row}");
}

//! G266 (NEW-10). A live change hears of a landing on its declared paths.
//!
//! Change A starts on `api/`, and change B on `api/` too. B lands. A's session is told once, at
//! its next `post-tool`, through the notice path of the refusals and early conflicts: one
//! `landed_on_your_paths` line naming B, its intent and actor, and the shared path `api/`, and
//! saying to refresh; the session's line names B in `landed_on_your_paths`. The next hook tells
//! nothing. A change on `ui/` hears of nothing. The paths are judged as `bay start` judges its
//! overlaps (`crate::overlap::shared`).
//!
//! The guard: in `crate::notice::landings`, `if paths.is_empty() { continue; }`. Mutate it
//! (remove the `continue`, or the check) and the change on `ui/` is told of B's landing too.

// Each case runs the hook with a wrapper on PATH, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

/// `bay start <intent> --paths <paths>` as `actor`: the change id and its view.
fn start(
    test_box: &TestBox,
    repo: &Path,
    actor: &str,
    intent: &str,
    paths: &[&str],
) -> (String, PathBuf) {
    let mut args = vec!["start", intent, "--paths"];
    args.extend(paths);
    let (code, started) = result_json(test_box.bay(repo).env("BAY_ACTOR", actor).args(&args));
    assert_eq!(code, 0, "{started}");
    assert_eq!(started["decision"], "ok", "{started}");
    (
        started["change"].as_str().unwrap().to_owned(),
        PathBuf::from(started["detail"]["view"].as_str().unwrap()),
    )
}

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

/// `bay hook <event>` with fixture `name` for `session` in `cwd`: its stdout and stderr.
fn hook(
    test_box: &TestBox,
    cwd: &Path,
    event: &str,
    name: &str,
    session: &str,
) -> (String, String) {
    let payload = fixture(name, session, cwd);
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
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

/// The notice `post-tool` gives the session's agent.
fn post_tool(test_box: &TestBox, view: &Path, session: &str) -> String {
    hook(
        test_box,
        view,
        "post-tool",
        "08-PostToolBatch.json",
        session,
    )
    .1
}

fn last_line(repo: &Path, session: &str) -> Value {
    let text =
        std::fs::read_to_string(repo.join(format!(".git/bay/sessions/{session}.jsonl"))).unwrap();
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

#[test]
fn a_landing_on_the_declared_paths_is_told_once_and_not_on_other_paths() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("api/a.rs", "fn a() {}\n"), ("ui/u.rs", "fn u() {}\n")],
        &unchecked_files(),
    );

    let (a, a_view) = start(&test_box, &repo, "agent-a", "PATHS-A", &["api/"]);
    let (_, ui_view) = start(&test_box, &repo, "agent-c", "PATHS-C", &["ui/"]);
    hook(
        &test_box,
        &a_view,
        "session-start",
        "01-SessionStart.json",
        "s-a",
    );
    hook(
        &test_box,
        &ui_view,
        "session-start",
        "01-SessionStart.json",
        "s-c",
    );
    // Nothing landed yet: no notice.
    assert_eq!(post_tool(&test_box, &a_view, "s-a"), "");
    assert_eq!(post_tool(&test_box, &ui_view, "s-c"), "");

    // B starts on the same paths and lands.
    let (b, b_view) = start(&test_box, &repo, "agent-b", "PATHS-B", &["api/"]);
    std::fs::write(b_view.join("api/b.rs"), "fn b() {}\n").unwrap();
    let (code, landed) = result_json(test_box.bay(&b_view).args(["land", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
    // A fetching command brings the landing here.
    let _ = result_json(test_box.bay(&repo).arg("status"));

    // A hears of it once, naming B, its intent and actor, the shared path and the refresh.
    let told = post_tool(&test_box, &a_view, "s-a");
    assert_eq!(told.lines().count(), 1, "{told}");
    assert!(
        told.starts_with(&format!(
            "bay: change {a}: landed_on_your_paths: change {b} (intent PATHS-B, actor agent-b) \
             landed on trunk on api/: refresh (`bay start --resume {a}`)"
        )),
        "{told}"
    );
    assert_eq!(
        last_line(&repo, "s-a")["landed_on_your_paths"],
        serde_json::json!([b])
    );
    // Once: not at the next step, not at the next prompt.
    assert_eq!(post_tool(&test_box, &a_view, "s-a"), "");
    assert!(
        last_line(&repo, "s-a")
            .get("landed_on_your_paths")
            .is_none()
    );
    let (context, prompt) = hook(
        &test_box,
        &a_view,
        "user-prompt-submit",
        "02-UserPromptSubmit.json",
        "s-a",
    );
    assert_eq!((context.as_str(), prompt.as_str()), ("", ""));

    // The change on `ui/` hears of nothing.
    assert_eq!(post_tool(&test_box, &ui_view, "s-c"), "");
    assert!(
        last_line(&repo, "s-c")
            .get("landed_on_your_paths")
            .is_none()
    );
}

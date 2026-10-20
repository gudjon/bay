//! G165. A killed session loses at most one interval (`plans/wave2.md` G165; `DESIGN` § 4,
//! § 11a).
//!
//! Ruling R7's stop refusal covers only a clean stop: a session that is killed never stops. So
//! the agent hook checkpoints the view of the session's change at the session's `SessionEnd`,
//! and at any hook event `[hooks] checkpoint_idle_s` (default 600) after the session's last
//! checkpoint, and pushes the checkpoint like any other (`bay start --resume`'s checkpoint,
//! without its fetch of trunk). The hook never waits for it: one detached `bay live checkpoint`
//! does the work, and the session's line names it (`checkpoint`).

// Each case runs the hook as a process with a payload on stdin, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::TestBox;

/// A probed Claude Code payload, its redacted session id and cwd rewritten to the test's.
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

/// `bay hook <event>` with the fixture `name` for `session` in `view`; asserts exit 0.
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

fn last_line(repo: &Path, session: &str) -> Value {
    let path = repo.join(format!(".git/bay/sessions/{session}.jsonl"));
    let text = std::fs::read_to_string(&path).unwrap();
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

/// Whether the remote's chain of change `id` holds a latest draft whose `file` reads `text`.
fn remote_draft_holds(test_box: &TestBox, bare: &Path, id: &str, file: &str, text: &str) -> bool {
    test_box
        .git(test_box.path())
        .arg(format!("--git-dir={}", bare.display()))
        .args(["show", &format!("refs/bay/changes/{id}^2:{file}")])
        .output()
        .is_ok_and(|out| out.status.success() && out.stdout == text.as_bytes())
}

/// Waits (at most 120 s, ending as soon as it holds) for the remote to hold the draft.
fn wait_for_draft(test_box: &TestBox, bare: &Path, id: &str, file: &str, text: &str) -> bool {
    let began = Instant::now();
    while began.elapsed() < Duration::from_secs(120) {
        if remote_draft_holds(test_box, bare, id, file, text) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

#[test]
fn a_session_end_puts_the_draft_on_the_remote() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let bare = test_box.path().join("repo-remote.git");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    let (id, view) = test_box.start_and_edit(&repo);
    hook(
        &test_box,
        &view,
        "session-start",
        "01-SessionStart.json",
        "s-end",
    );
    std::fs::write(view.join("src/a.rs"), "fn a() { 1; }\n").unwrap();

    // A step inside the interval (default 600 s) checkpoints nothing.
    hook(
        &test_box,
        &view,
        "post-tool",
        "08-PostToolBatch.json",
        "s-end",
    );
    assert!(last_line(&repo, "s-end").get("checkpoint").is_none());
    assert!(!remote_draft_holds(
        &test_box,
        &bare,
        &id,
        "src/a.rs",
        "fn a() { 1; }\n"
    ));

    hook(
        &test_box,
        &view,
        "session-end",
        "16-SessionEnd.json",
        "s-end",
    );
    let line = last_line(&repo, "s-end");
    assert_eq!(line["event"], "session_end", "{line}");
    assert_eq!(line["checkpoint"], "session_end", "{line}");
    assert!(
        wait_for_draft(&test_box, &bare, &id, "src/a.rs", "fn a() { 1; }\n"),
        "the session's edits never reached the remote"
    );
    assert!(remote_draft_holds(
        &test_box,
        &bare,
        &id,
        "src/new.rs",
        "fn new() {}\n"
    ));
}

#[test]
fn an_idle_interval_puts_the_draft_on_the_remote_without_a_stop() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let bare = test_box.path().join("repo-remote.git");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    test_box.commit_config(&repo, "[hooks]\ncheckpoint_idle_s = 1\n");
    let (id, view) = test_box.start_and_edit(&repo);
    hook(
        &test_box,
        &view,
        "session-start",
        "01-SessionStart.json",
        "s-idle",
    );
    std::fs::write(view.join("src/a.rs"), "fn a() { 2; }\n").unwrap();

    // No stop and no end: the session's next event after the interval checkpoints its view.
    std::thread::sleep(Duration::from_millis(2100));
    hook(
        &test_box,
        &view,
        "post-tool",
        "08-PostToolBatch.json",
        "s-idle",
    );
    let line = last_line(&repo, "s-idle");
    assert_eq!(line["checkpoint"], "idle", "{line}");
    assert!(
        wait_for_draft(&test_box, &bare, &id, "src/a.rs", "fn a() { 2; }\n"),
        "the session's edits never reached the remote"
    );
}

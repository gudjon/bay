//! G101. A live claim in the same tree denies a held path.
//!
//! `DESIGN` § 11c, "Two sessions in one tree" (owner ruling, 2026-10-19): every change has its
//! own isolated view, but two live sessions can still run in one tree (the main checkout, or a
//! second session opened inside a view). There a session claims each path it writes (path,
//! session, deadline, in the tree's one claims file), and `pre-tool` on `Edit`, `Write` and
//! `NotebookEdit`, and on `Bash` when the command names a path, denies a path another live
//! session of the same tree holds, with the reason code, the holder and the path. A claim past
//! its deadline is not live, the holder's stop releases its claims, a subagent works under its
//! parent's claims, and a claim never refuses a write in another view. A claims file bay cannot
//! read allows the write with a `COVERAGE_UNKNOWN` notice recorded in the session's row (ruling
//! R5), never silently.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};
use support::TestBox;

/// `bay hook <event>` with `payload` on stdin; returns (exit, stderr).
fn hook(test_box: &TestBox, cwd: &Path, event: &str, payload: &Value) -> (Option<i32>, String) {
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
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A `PreToolUse` payload of `tool` with `input`, in the recorded shape (fixture 03).
fn pre(session: &str, agent: Option<&str>, cwd: &Path, tool: &str, input: &Value) -> Value {
    let mut payload = json!({
        "session_id": session,
        "hook_event_name": "PreToolUse",
        "tool_name": tool,
        "tool_input": input,
        "cwd": cwd,
    });
    if let Some(agent) = agent {
        payload["agent_id"] = json!(agent);
    }
    payload
}

fn write_tool(session: &str, agent: Option<&str>, cwd: &Path, file: &Path) -> Value {
    pre(
        session,
        agent,
        cwd,
        "Write",
        &json!({"file_path": file, "content": "x"}),
    )
}

fn bash(session: &str, cwd: &Path, command: &str) -> Value {
    pre(session, None, cwd, "Bash", &json!({"command": command}))
}

/// The `PostToolBatch` (fixture 08's shape) of one step of `session` (of its subagent `agent`)
/// whose calls were Writes of `files`.
fn batch(test_box: &TestBox, session: &str, agent: Option<&str>, cwd: &Path, files: &[&Path]) {
    let calls: Vec<Value> = files
        .iter()
        .map(|file| {
            json!({
                "tool_name": "Write",
                "tool_input": {"file_path": file, "content": "x"},
                "tool_use_id": "toolu_g101",
                "tool_response": "File created successfully",
            })
        })
        .collect();
    let mut batch = json!({
        "session_id": session,
        "hook_event_name": "PostToolBatch",
        "cwd": cwd,
        "tool_calls": calls,
    });
    if let Some(agent) = agent {
        batch["agent_id"] = json!(agent);
    }
    assert_eq!(hook(test_box, cwd, "post-tool", &batch).0, Some(0));
}

/// One step of `session` (of its subagent `agent`) that writes `file`: the file is written, then
/// the step's batch records it.
fn writes(test_box: &TestBox, session: &str, agent: Option<&str>, cwd: &Path, file: &Path) {
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, format!("fn {}() {{}}\n", session.replace('-', "_"))).unwrap();
    batch(test_box, session, agent, cwd, &[file]);
}

/// The session's own stop, after the product already continued it once (not refused, R7).
fn stops(test_box: &TestBox, session: &str, cwd: &Path) {
    let stop = json!({
        "session_id": session,
        "hook_event_name": "Stop",
        "stop_hook_active": true,
        "cwd": cwd,
    });
    assert_eq!(hook(test_box, cwd, "stop", &stop).0, Some(0));
}

fn setup(config: Option<&str>) -> (TestBox, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    if let Some(config) = config {
        TestBox::bay_config(&repo, &format!("{}{config}", support::MINIMAL_CONFIG));
    }
    test_box.seed_trunk(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), ("src/b.rs", "fn b() {}\n")],
    );
    (test_box, repo)
}

fn view_of(test_box: &TestBox, repo: &Path, intent: &str) -> PathBuf {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", "src/"]);
    PathBuf::from(started["detail"]["view"].as_str().unwrap())
        .canonicalize()
        .unwrap()
}

fn assert_held(outcome: &(Option<i32>, String), holder: &str, path: &str) {
    let (code, stderr) = outcome;
    assert_eq!(*code, Some(2), "not denied: {stderr}");
    assert!(stderr.contains("PATH_HELD"), "{stderr}");
    assert!(stderr.contains(holder), "{stderr}");
    assert!(stderr.contains(path), "{stderr}");
}

fn assert_allowed(outcome: &(Option<i32>, String)) {
    assert_eq!(outcome.0, Some(0), "denied: {}", outcome.1);
}

/// The session's work record rows, from the main repository's Git directory.
fn rows(repo: &Path, session: &str) -> Vec<Value> {
    let path = repo.join(format!(".git/bay/sessions/{session}.jsonl"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn two_sessions_in_one_view_a_held_path_is_denied_and_an_unheld_path_is_allowed() {
    let (test_box, repo) = setup(None);
    let view = view_of(&test_box, &repo, "CARD-1");
    let held = view.join("src/a.rs");
    writes(&test_box, "s-holder", None, &view, &held);

    let denied = hook(
        &test_box,
        &view,
        "pre-tool",
        &write_tool("s-other", None, &view, &held),
    );
    assert_held(&denied, "s-holder", "src/a.rs");
    // A relative target names the same file.
    assert_held(
        &hook(
            &test_box,
            &view,
            "pre-tool",
            &pre(
                "s-other",
                None,
                &view,
                "Edit",
                &json!({"file_path": "src/../src/a.rs", "old_string": "a", "new_string": "b"}),
            ),
        ),
        "s-holder",
        "src/a.rs",
    );
    assert_allowed(&hook(
        &test_box,
        &view,
        "pre-tool",
        &write_tool("s-other", None, &view, &view.join("src/b.rs")),
    ));
    // The holder itself writes its own path.
    assert_allowed(&hook(
        &test_box,
        &view,
        "pre-tool",
        &write_tool("s-holder", None, &view, &held),
    ));

    // The deny is a G60 verdict row of the denied session, with the holder and the path.
    let row = rows(&repo, "s-other")
        .into_iter()
        .find(|row| row["event"] == "pre_tool_use")
        .expect("no pre-tool row");
    assert_eq!(row["state"], "blocked(PATH_HELD)", "{row}");
    assert_eq!(row["verdict"]["decision"], "deny", "{row}");
    assert_eq!(row["verdict"]["reason"], "PATH_HELD", "{row}");
    assert_eq!(row["refused"]["holder"], "s-holder", "{row}");
    assert_eq!(row["refused"]["path"], "src/a.rs", "{row}");

    // The other session's batch lists its denied write of the held path beside its own write
    // of another: it claims only the one it may hold, so the holder still writes its path.
    let other = view.join("src/b.rs");
    std::fs::write(&other, "fn other() {}\n").unwrap();
    batch(&test_box, "s-other", None, &view, &[&held, &other]);
    assert_allowed(&hook(
        &test_box,
        &view,
        "pre-tool",
        &write_tool("s-holder", None, &view, &held),
    ));
    assert_held(
        &hook(
            &test_box,
            &view,
            "pre-tool",
            &write_tool("s-holder", None, &view, &view.join("src/b.rs")),
        ),
        "s-other",
        "src/b.rs",
    );

    // The holder's stop releases its claims.
    stops(&test_box, "s-holder", &view);
    assert_allowed(&hook(
        &test_box,
        &view,
        "pre-tool",
        &write_tool("s-other", None, &view, &held),
    ));
}

#[test]
fn two_sessions_in_the_main_checkout_are_judged_the_same_way() {
    let (test_box, repo) = setup(None);
    let repo = repo.canonicalize().unwrap();
    let held = repo.join("src/a.rs");
    writes(&test_box, "s-holder", None, &repo, &held);
    assert_held(
        &hook(
            &test_box,
            &repo,
            "pre-tool",
            &write_tool("s-other", None, &repo, &held),
        ),
        "s-holder",
        "src/a.rs",
    );
    assert_allowed(&hook(
        &test_box,
        &repo,
        "pre-tool",
        &write_tool("s-other", None, &repo, &repo.join("src/b.rs")),
    ));
}

#[test]
fn a_past_deadline_claim_is_not_live() {
    // A claim lives for the takeover window; a short one here.
    let (test_box, repo) = setup(Some("[executor]\nlease_s = 1\ntakeover_s = 3\n"));
    let view = view_of(&test_box, &repo, "CARD-1");
    let held = view.join("src/a.rs");
    writes(&test_box, "s-holder", None, &view, &held);
    let claimed = std::time::Instant::now();
    let denied = hook(
        &test_box,
        &view,
        "pre-tool",
        &write_tool("s-other", None, &view, &held),
    );
    if claimed.elapsed() < std::time::Duration::from_secs(2) {
        assert_held(&denied, "s-holder", "src/a.rs");
    }
    std::thread::sleep(std::time::Duration::from_secs(4).saturating_sub(claimed.elapsed()));
    assert_allowed(&hook(
        &test_box,
        &view,
        "pre-tool",
        &write_tool("s-other", None, &view, &held),
    ));
}

#[test]
fn a_bash_command_that_names_a_held_path_is_denied() {
    let (test_box, repo) = setup(None);
    let view = view_of(&test_box, &repo, "CARD-1");
    let held = view.join("src/a.rs");
    writes(&test_box, "s-holder", None, &view, &held);
    for command in [
        "echo x > src/a.rs".to_owned(),
        "echo x >>src/a.rs".to_owned(),
        "sed -i 's/a/b/' ./src/a.rs".to_owned(),
        format!("cp /dev/null {}", held.display()),
        "cd . && bash -c 'cat /dev/null > src/a.rs'".to_owned(),
    ] {
        assert_held(
            &hook(
                &test_box,
                &view,
                "pre-tool",
                &bash("s-other", &view, &command),
            ),
            "s-holder",
            "src/a.rs",
        );
    }
    for command in ["echo x > src/b.rs", "ls src", "cat src/a.rs.bak"] {
        assert_allowed(&hook(
            &test_box,
            &view,
            "pre-tool",
            &bash("s-other", &view, command),
        ));
    }
    assert_allowed(&hook(
        &test_box,
        &view,
        "pre-tool",
        &bash("s-holder", &view, "echo x > src/a.rs"),
    ));
}

#[test]
fn the_same_path_in_another_view_is_allowed() {
    let (test_box, repo) = setup(None);
    let one = view_of(&test_box, &repo, "CARD-1");
    let two = view_of(&test_box, &repo, "CARD-2");
    writes(&test_box, "s-holder", None, &one, &one.join("src/a.rs"));
    // The same path of another tree, from that tree and from the holder's.
    assert_allowed(&hook(
        &test_box,
        &two,
        "pre-tool",
        &write_tool("s-other", None, &two, &two.join("src/a.rs")),
    ));
    assert_allowed(&hook(
        &test_box,
        &two,
        "pre-tool",
        &bash("s-other", &two, "echo x > src/a.rs"),
    ));
    assert_allowed(&hook(
        &test_box,
        &repo,
        "pre-tool",
        &write_tool("s-other", None, &repo, &repo.join("src/a.rs")),
    ));
    // In the holder's own tree it is still held.
    assert_held(
        &hook(
            &test_box,
            &one,
            "pre-tool",
            &write_tool("s-other", None, &one, &one.join("src/a.rs")),
        ),
        "s-holder",
        "src/a.rs",
    );
}

#[test]
fn a_subagent_of_the_holder_is_allowed() {
    let (test_box, repo) = setup(None);
    let view = view_of(&test_box, &repo, "CARD-1");
    let held = view.join("src/a.rs");
    writes(&test_box, "s-holder", None, &view, &held);
    assert_allowed(&hook(
        &test_box,
        &view,
        "pre-tool",
        &write_tool("s-holder", Some("agent-sub"), &view, &held),
    ));
    // What the subagent writes is held for its parent's session.
    writes(
        &test_box,
        "s-holder",
        Some("agent-sub"),
        &view,
        &view.join("src/c.rs"),
    );
    assert_held(
        &hook(
            &test_box,
            &view,
            "pre-tool",
            &write_tool("s-other", None, &view, &view.join("src/c.rs")),
        ),
        "s-holder",
        "src/c.rs",
    );
}

/// `bay hook pre-tool` with `payload` on stdin; returns (exit, stdout, stderr).
fn pre_tool_output(
    test_box: &TestBox,
    cwd: &Path,
    payload: &Value,
) -> (Option<i32>, Value, String) {
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", "pre-tool"])
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
    let stdout = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (
        out.status.code(),
        stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn an_unreadable_claims_file_allows_with_a_recorded_coverage_unknown_notice() {
    // Ruling R5: a write bay cannot judge is allowed with a COVERAGE_UNKNOWN notice, recorded in
    // the session's row, never silently.
    let (test_box, repo) = setup(None);
    let repo = repo.canonicalize().unwrap();
    std::fs::create_dir_all(repo.join(".git/bay")).unwrap();
    std::fs::write(repo.join(".git/bay/claims.json"), "not json").unwrap();
    let target = repo.join("src/a.rs");
    for payload in [
        write_tool("s-other", None, &repo, &target),
        bash("s-other", &repo, "echo x > src/a.rs"),
    ] {
        let (code, stdout, stderr) = pre_tool_output(&test_box, &repo, &payload);
        assert_eq!(code, Some(0), "denied: {stderr}");
        assert!(stderr.contains("COVERAGE_UNKNOWN"), "{stderr}");
        assert!(stderr.contains("claims"), "{stderr}");
        let context = stdout["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_default();
        assert!(context.contains("COVERAGE_UNKNOWN"), "{stdout}");
    }
    let notices: Vec<Value> = rows(&repo, "s-other")
        .into_iter()
        .filter(|row| row["event"] == "pre_tool_use")
        .collect();
    assert_eq!(notices.len(), 2, "{notices:?}");
    for row in &notices {
        assert_eq!(row["notice"]["reason"], "COVERAGE_UNKNOWN", "{row}");
        assert!(row["notice"]["claims_file"].is_string(), "{row}");
        assert_eq!(row["verdict"]["decision"], "allow", "{row}");
        assert_eq!(row["verdict"]["reason"], "COVERAGE_UNKNOWN", "{row}");
    }
    // The Write names its path; a shell command's words are not known to be its writes.
    assert_eq!(notices[0]["notice"]["path"], "src/a.rs", "{notices:?}");
    assert!(notices[1]["notice"]["path"].is_null(), "{notices:?}");

    // A readable file judges as before.
    std::fs::remove_file(repo.join(".git/bay/claims.json")).unwrap();
    writes(&test_box, "s-holder", None, &repo, &target);
    assert_held(
        &hook(
            &test_box,
            &repo,
            "pre-tool",
            &write_tool("s-other", None, &repo, &target),
        ),
        "s-holder",
        "src/a.rs",
    );
}

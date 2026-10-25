//! G357 (owner ruling 2026-10-25, dossier 63 § 6 item 1): bay records why a session waits.
//!
//! A session that needs a person shows only "waiting", and nothing says for what. Claude Code
//! 2.1.310 (`tests/fixtures/claude-code-2.1.310`) says it: a `Notification` names its kind
//! (`permission_prompt`, `idle_prompt`) and a message, and a question tool (`AskUserQuestion`,
//! `ExitPlanMode`) reaches a `PreToolUse` hook. `bay install` writes a second, record-only
//! `PreToolUse` group for the question tools (`bay hook tool.ask`: no `onFailure`, no wait), the
//! session's row in `bay status --all` names what it waits for, and the question and the message
//! stay in the session's own record, never in the live row. With no answer the hook says nothing,
//! so the product's dialog opens as it would without bay; and no other tool passes under
//! `tool.ask`.
//!
//! Guards to mutate:
//! - `src/agent_settings.rs` `CLAUDE_EVENTS`: drop `("PreToolUse", "tool.ask")` (no group asks).
//! - `src/agent_hook.rs` `misnamed_tool_call`: let `tool.ask` take any tool (a shell call passes
//!   under it).
//! - `src/live.rs` `ROW_FIELDS`: drop `"waiting_for"` (the map says only "waiting").

use crate::support;

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::{TestBox, UNCHECKED_POLICY, result_json};

/// A recorded Claude Code 2.1.310 payload `name`, for `session` in `cwd`.
fn fixture(name: &str, session: &str, cwd: &Path) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude-code-2.1.310")
        .join(name);
    let mut payload: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    payload["session_id"] = json!(session);
    payload["cwd"] = json!(cwd);
    payload
}

/// `bay hook <event>` of `payload` from `cwd`: (exit, stdout).
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
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// The map's row of `session`, after this machine's presence is pushed and fetched.
fn row(test_box: &TestBox, repo: &Path, session: &str) -> Value {
    test_box.bay_ok(repo, &["live", "push"]);
    test_box.bay_ok(repo, &["status"]);
    let (code, map) = result_json(test_box.bay(repo).args(["status", "--all"]));
    assert_eq!(code, 0, "{map}");
    map["detail"]["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("no sessions: {map}"))
        .iter()
        .find(|row| row["session"] == session)
        .cloned()
        .unwrap_or_else(|| panic!("no row of {session}: {map}"))
}

/// The last line of `session`'s own record.
fn last_line(repo: &Path, session: &str) -> Value {
    let text =
        std::fs::read_to_string(repo.join(format!(".git/bay/sessions/{session}.jsonl"))).unwrap();
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

fn seeded(test_box: &TestBox) -> std::path::PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[(".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned())],
    );
    repo
}

#[test]
fn install_writes_a_record_only_group_for_the_question_tools() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, "main = \"main\"\n\n[agent]\nproduct = \"claude\"\n");
    test_box.fake_claude(Some("2.1.310"));
    test_box.bay_ok(&repo, &["install"]);
    let text = std::fs::read_to_string(repo.join(".claude/settings.json")).unwrap();
    let settings: Value = serde_json::from_str(&text).unwrap();
    let groups = settings["hooks"]["PreToolUse"].as_array().unwrap();
    let runs = |group: &Value, event: &str| {
        group["hooks"][0]["command"]
            .as_str()
            .is_some_and(|command| command.ends_with(&format!("bay hook {event}")))
    };
    let ask = groups
        .iter()
        .find(|group| runs(group, "tool.ask"))
        .unwrap_or_else(|| panic!("no question group: {settings}"));
    assert_eq!(ask["matcher"], "AskUserQuestion|ExitPlanMode", "{ask}");
    assert!(ask["hooks"][0].get("onFailure").is_none(), "{ask}");
    // Above the longest wait the policy allows (90 s), so a hung bay never holds the agent for the
    // product's default.
    assert_eq!(ask["hooks"][0]["timeout"], 120, "{ask}");
    let before = groups
        .iter()
        .find(|group| runs(group, "tool.before"))
        .unwrap_or_else(|| panic!("the pre-tool group is gone: {settings}"));
    assert_eq!(before["hooks"][0]["onFailure"], "block", "{before}");
}

#[test]
fn a_question_is_recorded_and_its_dialog_opens_as_without_bay() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let ask = fixture("01-PreToolUse-AskUserQuestion.json", "s-ask", &repo);
    let (code, stdout) = hook(&test_box, &repo, "tool.ask", &ask);
    assert_eq!(code, Some(0));
    assert_eq!(stdout, "", "the hook answered for the person");
    let line = last_line(&repo, "s-ask");
    assert_eq!(line["state"], "waiting", "{line}");
    assert_eq!(
        line["question"]["questions"][0]["question"], "Which colour should the button be?",
        "{line}"
    );
    let row = row(&test_box, &repo, "s-ask");
    assert_eq!(
        row["waiting_for"],
        json!({"kind": "question", "tool": "AskUserQuestion"}),
        "{row}"
    );
    assert!(
        row.get("question").is_none(),
        "the question left the machine: {row}"
    );
}

#[test]
fn a_notification_names_its_kind_and_keeps_its_message_here() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let idle = fixture("14-Notification-idle_prompt.json", "s-idle", &repo);
    assert_eq!(hook(&test_box, &repo, "session.wait", &idle).0, Some(0));
    assert_eq!(
        last_line(&repo, "s-idle")["message"],
        "Claude is waiting for your input"
    );
    let row = row(&test_box, &repo, "s-idle");
    assert_eq!(row["waiting_for"], json!({"kind": "idle_prompt"}), "{row}");
    assert!(row.get("message").is_none(), "{row}");
}

#[test]
fn no_other_tool_passes_under_the_question_entry() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let mut shell = fixture("01-PreToolUse-AskUserQuestion.json", "s-ask", &repo);
    shell["tool_name"] = json!("Bash");
    shell["tool_input"] = json!({"command": "git push --force origin main"});
    let (code, _) = hook(&test_box, &repo, "tool.ask", &shell);
    assert_ne!(code, Some(0), "a shell call passed under tool.ask");
}

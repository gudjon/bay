//! G358 (owner ruling 2026-10-25, dossier 63 § 6 item 2): a question is a talk the session opens
//! and ends.
//!
//! A question an agent asks with its question tool (`AskUserQuestion`, `ExitPlanMode`) is a
//! signed record a person can see and, later, answer from elsewhere: under a key registry, the
//! `tool.ask` hook opens one discovery talk per question, from the session's actor to itself,
//! `decision: question:<tool_use_id>:<n>` in the tool's order, its options and `multi_select` as
//! data. The live row names the talks (`waiting_for.talks`), never their text. When the session
//! goes on (the question answered at the terminal, or the next step), its open question talks
//! close as `ended`, so nothing offers an answer that can no longer reach the agent. Without a
//! registry no talk is sent (G357).
//!
//! Guards to mutate:
//! - `src/agent_hook.rs` `question_talks`: return no talks (the host holds no question).
//! - `src/agent_hook.rs` `dispatch_as`: drop the `end_questions` call (the talk stays open).

use crate::support;

use std::io::Write as _;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::talk::hosted_talk;

/// A recorded Claude Code payload `name` of the fixture folder `dir`, for `session` in `cwd`.
fn fixture(dir: &str, name: &str, session: &str, cwd: &Path) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
        .join(name);
    let mut payload: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    payload["session_id"] = json!(session);
    payload["cwd"] = json!(cwd);
    payload
}

/// `bay hook <event>` of `payload` as `agent-b` on its clone; asserts it exits 0 with no stdout
/// (the product's own dialog opens).
fn hook(case: &support::talk::TalkCase, event: &str, payload: &Value) {
    let mut child = case
        .test_box
        .bay_process(&case.b)
        .env("BAY_ACTOR", "agent-b")
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
    if event == "tool.ask" {
        assert!(out.stdout.is_empty(), "the hook answered: {out:?}");
    }
}

/// The talk refs the host holds for `agent-b`.
fn host_talks(case: &support::talk::TalkCase) -> Vec<String> {
    case.test_box
        .git_bare(
            &case.host,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/bay/talk/agent-b/",
            ],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_question_opens_a_talk_and_the_next_step_ends_it() {
    let case = hosted_talk();
    let ask = fixture(
        "claude-code-2.1.310",
        "01-PreToolUse-AskUserQuestion.json",
        "s-q",
        &case.b,
    );
    hook(&case, "tool.ask", &ask);

    let talks = host_talks(&case);
    assert_eq!(talks.len(), 1, "the host holds no question talk: {talks:?}");
    let body: Value = serde_json::from_str(
        &case
            .test_box
            .git_bare(&case.host, &["show", &format!("{}:talk.json", talks[0])]),
    )
    .unwrap();
    assert_eq!(body["kind"], "discovery", "{body}");
    assert_eq!(body["who"], "agent-b", "{body}");
    assert_eq!(body["what"], "Which colour should the button be?", "{body}");
    assert_eq!(
        body["decision"],
        format!("question:{}:0", ask["tool_use_id"].as_str().unwrap()),
        "{body}"
    );
    assert_eq!(body["multi_select"], false, "{body}");
    assert_eq!(body["options"][1]["label"], "Blue", "{body}");
    let text = std::fs::read_to_string(case.b.join(".git/bay/sessions/s-q.jsonl")).unwrap();
    let line: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(line["waiting_for"]["talks"], json!([body["id"]]), "{line}");

    // The session goes on: its next prompt ends the question talk on the host.
    let prompt = fixture(
        "claude-code-2.1.302",
        "02-UserPromptSubmit.json",
        "s-q",
        &case.b,
    );
    hook(&case, "turn.start", &prompt);
    let entries = case.test_box.git_bare(
        &case.host,
        &["ls-tree", "-r", "--name-only", &talks[0], "entries/"],
    );
    let close = entries
        .lines()
        .find(|path| path.ends_with("-close.json"))
        .unwrap_or_else(|| panic!("the question talk stayed open: {entries}"));
    let close: Value = serde_json::from_str(
        &case
            .test_box
            .git_bare(&case.host, &["show", &format!("{}:{close}", talks[0])]),
    )
    .unwrap();
    assert_eq!(close["state"], "ended", "{close}");
}

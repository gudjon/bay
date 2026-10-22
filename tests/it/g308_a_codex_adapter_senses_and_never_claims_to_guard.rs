//! G308. A Codex adapter senses, and never claims to guard.
//!
//! With `[agent] product = "codex"`, `bay hook <event>` reads the payloads recorded from Codex
//! 0.153.2 (`tests/fixtures/codex-0.153.2`) through Codex's own payload module and feeds them to
//! the event handling the Claude Code adapter uses: a pre-tool `git push` of the trunk is denied
//! with the same exit and the same reason as Claude Code's, an unreadable pre-tool payload is
//! denied with `HOOK_INPUT_INVALID` (ruling R-H1) while the events that only record allow one,
//! every recorded event is answered, and Codex's `PostToolUse` ends a step (Codex has no
//! `PostToolBatch`). `bay install` writes bay's entries into `.codex/hooks.json`, only for the
//! recorded events, and leaves another tool's entry where it was. Codex runs a tool whose hook
//! crashes, hangs or fails, so doctor's `agent-hooks` row says `early_refusal: "fail_open"`: the guard stays at
//! main (land and the trunk hook), and doctor judges Codex against its floor, 0.153.2.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/agent_hook.rs` `Product::configured`: `Ok(repo) if repo.config.codex_adapter() =>
//!   Self::Codex,` (make it `Self::Claude`): an `apply_patch` call with no command is read as
//!   an unjudged tool and allowed, and Codex's `PostToolUse` no longer ends a step.
//! - `src/agent_hook.rs` `Product::step_event`: `Self::Codex => Event::PostToolUse,` (make it
//!   `Event::PostToolBatch`): the session's `post_tool_use` row is not `working`.
//! - `src/agent_hook/codex.rs` `event`: `"PostToolUse" => Event::PostToolUse,` (delete it): the
//!   recorded `PostToolUse` has no behaviour and writes no row.
//! - `src/agent_hook/codex.rs` `edit_targets`: `.ok_or("is an apply_patch call with no string
//!   tool_input.command")?;` (make it `.unwrap_or_default();`): that call is allowed.
//! - `src/install.rs` `run`: `Adapter::Codex => agent_settings::add_codex_entries(&mut
//!   settings),` (make it `add_claude_entries`): `.codex/hooks.json` holds Claude-only events.
//! - `src/agent_settings.rs` `CODEX_EVENTS`: `("PreToolUse", "tool.before"),` (delete it): no
//!   pre-tool entry is written, and doctor's `hooks` row is red.
//! - `src/doctor.rs` `codex_hooks_rows`: `if present { "fail_open" }` (make it `"fail_closed"`): the row claims
//!   a guard Codex does not give.
//! - `src/doctor.rs` `codex_row`: `Program::Found(version) if *version < minimum => {` (make it
//!   `if false`): a Codex below its floor is green.

// Each case runs shell scripts (a fake codex on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::{TestBox, result_json, row, write_executable};

const CODEX_ADAPTER: &str = "trunk = \"main\"\n\n[agent]\nproduct = \"codex\"\n";
const CLAUDE_ADAPTER: &str = "trunk = \"main\"\n\n[agent]\nproduct = \"claude\"\n";

/// Every recorded Codex event, with the `bay hook` event bay installs for it.
const CODEX_EVENTS: [(&str, &str); 8] = [
    ("SessionStart", "session.start"),
    ("UserPromptSubmit", "turn.start"),
    ("PreToolUse", "tool.before"),
    ("PostToolUse", "step.end"),
    ("Stop", "turn.end"),
    ("SubagentStart", "subagent.start"),
    ("SubagentStop", "subagent.end"),
    ("SessionEnd", "session.end"),
];

const SESSION: &str = "s-g308";

/// A repository whose config at trunk names `config`'s adapter.
fn adapter_repo(test_box: &TestBox, config: &str) -> std::path::PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    test_box.commit_config(&repo, config);
    repo
}

/// Puts a `codex` that reports `version` on PATH.
fn fake_codex(test_box: &TestBox, version: &str) {
    write_executable(
        &test_box.fakebin.join("codex"),
        &format!("#!/bin/sh\necho \"codex-cli {version}\"\n"),
    );
}

/// A recorded payload of `product`, with this session and `repo` in place of the redacted
/// fields, and `repo` for `<repo>` in a command.
fn fixture(product: &str, name: &str, repo: &Path) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(product)
        .join(name);
    let mut payload: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    payload["session_id"] = json!(SESSION);
    payload["cwd"] = json!(repo);
    if let Some(command) = payload["tool_input"]["command"].as_str() {
        payload["tool_input"]["command"] =
            json!(command.replace("<repo>", &repo.display().to_string()));
    }
    payload
}

/// `bay hook <event>` with `input` on stdin; returns (exit, stdout, stderr).
fn hook(
    test_box: &TestBox,
    cwd: &Path,
    event: &str,
    input: &[u8],
) -> (Option<i32>, String, String) {
    let mut child = test_box
        .bay_process(cwd)
        .args(["hook", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The `bay hook` event bay installs for a recorded payload's `hook_event_name`.
fn bay_event(payload: &Value) -> &'static str {
    let name = payload["hook_event_name"].as_str().unwrap();
    CODEX_EVENTS
        .iter()
        .find(|(event, _)| *event == name)
        .map_or_else(
            || panic!("{name} is no recorded Codex event"),
            |(_, bay_event)| *bay_event,
        )
}

#[test]
fn a_trunk_push_is_denied_with_the_reason_claude_code_gets() {
    let push = "git push origin main";
    let mut reasons = Vec::new();
    for (config, product, name) in [
        (CODEX_ADAPTER, "codex-0.153.2", "05-PreToolUse-main.json"),
        (CLAUDE_ADAPTER, "claude-code-2.1.302", "05-PreToolUse.json"),
    ] {
        let test_box = TestBox::new();
        let repo = adapter_repo(&test_box, config);
        let mut payload = fixture(product, name, &repo);
        assert_eq!(payload["tool_name"], "Bash", "{product}: {payload}");
        payload["tool_input"]["command"] = json!(push);
        let (code, stdout, stderr) = hook(
            &test_box,
            &repo,
            "tool.before",
            payload.to_string().as_bytes(),
        );
        assert_eq!(code, Some(2), "{product}: {push} was allowed: {stderr}");
        assert!(stdout.is_empty(), "{product}: {stdout}");
        assert!(stderr.contains("bay denies"), "{product}: {stderr}");
        reasons.push(stderr);
    }
    assert_eq!(reasons[0], reasons[1], "the two products' reasons differ");
}

#[test]
fn an_unreadable_pre_tool_payload_is_denied_and_a_recording_event_allows_one() {
    let test_box = TestBox::new();
    let repo = adapter_repo(&test_box, CODEX_ADAPTER);
    let mut no_patch = fixture("codex-0.153.2", "03-PreToolUse-main.json", &repo);
    no_patch["tool_input"] = json!({});
    for (what, input) in [
        ("text that is not JSON", b"not json".to_vec()),
        ("a missing payload", Vec::new()),
        (
            "an apply_patch with no command",
            no_patch.to_string().into_bytes(),
        ),
    ] {
        let (code, stdout, stderr) = hook(&test_box, &repo, "tool.before", &input);
        assert_eq!(code, Some(2), "{what} was allowed: {stderr}");
        assert_eq!(stderr.lines().count(), 1, "{what}: {stderr}");
        assert!(stderr.contains("HOOK_INPUT_INVALID"), "{what}: {stderr}");
        let result: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("{what}: stdout {stdout:?}: {error}"));
        assert_eq!(result["reason"], "HOOK_INPUT_INVALID", "{what}: {result}");
    }
    for event in ["step.end", "stop", "session.end"] {
        let (code, _, stderr) = hook(&test_box, &repo, event, b"not json");
        assert_eq!(code, Some(0), "{event}: {stderr}");
    }
}

#[test]
fn every_recorded_event_is_answered_and_its_post_tool_ends_a_step() {
    let test_box = TestBox::new();
    let repo = adapter_repo(&test_box, CODEX_ADAPTER);
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex-0.153.2");
    let mut names: Vec<String> = std::fs::read_dir(&fixtures)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        })
        .collect();
    names.sort();
    assert_eq!(names.len(), 15, "{names:?}");
    for name in &names {
        let payload = fixture("codex-0.153.2", name, &repo);
        let event = bay_event(&payload);
        let (code, stdout, stderr) = hook(&test_box, &repo, event, payload.to_string().as_bytes());
        // bay judges nothing here to deny: the probe's own hook denied fixture 07.
        assert_eq!(code, Some(0), "{name}: {stdout} {stderr}");
        assert!(!stderr.contains("HOOK_INPUT_INVALID"), "{name}: {stderr}");
        if event != "session.start" {
            assert!(stdout.is_empty(), "{name}: Codex reads no stdout: {stdout}");
        }
    }
    let path = repo.join(format!(".git/bay/sessions/{SESSION}.jsonl"));
    let rows: Vec<Value> = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let steps: Vec<&Value> = rows
        .iter()
        .filter(|row| row["event"] == "post_tool_use")
        .collect();
    assert_eq!(steps.len(), 2, "fixtures 04 and 06: {rows:?}");
    for step in steps {
        assert_eq!(step["state"], "working", "{step}");
    }
}

#[test]
fn install_writes_the_recorded_codex_events_and_keeps_another_tools_entry() {
    let test_box = TestBox::new();
    let repo = adapter_repo(&test_box, CODEX_ADAPTER);
    let other =
        json!({"hooks": [{"type": "command", "command": "other-tool session", "timeout": 10}]});
    std::fs::create_dir_all(repo.join(".codex")).unwrap();
    std::fs::write(
        repo.join(".codex/hooks.json"),
        json!({"hooks": {"SessionStart": [other.clone()]}}).to_string(),
    )
    .unwrap();
    test_box.bay_ok(&repo, &["install"]);
    let result = test_box.bay_ok(&repo, &["install"]);
    assert_eq!(
        result["detail"]["unrelated_hooks_unchanged"], true,
        "{result}"
    );
    assert!(!repo.join(".claude/settings.json").exists());
    let text = std::fs::read_to_string(repo.join(".codex/hooks.json")).unwrap();
    let hooks: Value = serde_json::from_str(&text).unwrap();
    let events = hooks["hooks"].as_object().unwrap();
    let mut names: Vec<&str> = events.keys().map(String::as_str).collect();
    names.sort_unstable();
    let mut expected: Vec<&str> = CODEX_EVENTS.iter().map(|(event, _)| *event).collect();
    expected.sort_unstable();
    assert_eq!(names, expected, "only the recorded events: {hooks}");
    for (event, bay_event) in CODEX_EVENTS {
        let commands: Vec<&str> = events[event]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["hooks"].as_array().unwrap())
            .filter_map(|hook| hook["command"].as_str())
            .filter(|command| command.starts_with("bay hook "))
            .collect();
        assert_eq!(
            commands,
            [format!("bay hook {bay_event}")],
            "{event}: {hooks}"
        );
    }
    assert_eq!(hooks["hooks"]["SessionStart"][0], other, "{hooks}");
    let pre_tool = &hooks["hooks"]["PreToolUse"][0]["hooks"][0];
    assert_eq!(pre_tool["timeout"], 30, "{pre_tool}");
}

#[test]
fn doctor_says_codex_hooks_fail_open_and_judges_its_floor() {
    let test_box = TestBox::new();
    let repo = adapter_repo(&test_box, CODEX_ADAPTER);
    fake_codex(&test_box, "0.153.2");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "no entries yet: {result}");
    assert_eq!(result["reason"], "HOOKS_MISSING", "{result}");
    assert_eq!(
        row(&result, "agent-hooks")["early_refusal"],
        "none",
        "{result}"
    );

    test_box.bay_ok(&repo, &["install"]);
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let hooks = row(&result, "hooks");
    assert_eq!(hooks["product"], "codex", "{hooks}");
    assert_eq!(hooks["state"], "green", "{hooks}");
    assert_eq!(
        row(&result, "agent-hooks")["early_refusal"],
        "fail_open",
        "{result}"
    );
    let posture = row(&result, "agent-hooks");
    assert_eq!(posture["product"], "codex", "{posture}");
    assert_eq!(posture["early_refusal"], "fail_open", "{posture}");
    assert_eq!(
        posture["guarantee"], "land and trunk hook (G8, G23)",
        "{posture}"
    );
    let codex = row(&result, "codex");
    assert_eq!(codex["state"], "green", "{codex}");
    assert_eq!(codex["floor"], "0.153.2", "{codex}");

    fake_codex(&test_box, "0.152.9");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "TOOL_BELOW_FLOOR", "{result}");
    assert_eq!(result["detail"]["tool"], "codex", "{result}");
    assert_eq!(row(&result, "codex")["state"], "red", "{result}");
}

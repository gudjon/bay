//! G158. A hung or failing hook lets the call through within its deadline (owner ruling
//! 2026-11-20, Code Mode: "fail open and never hang").
//!
//! In Code Mode a pre-tool hook that fails or does not finish refuses the call, which stops the
//! program partway. bay denies only on purpose: `bay install` writes `"onFailure": "continue"` and
//! `"timeout": 30` on bay's `PreToolUse` entry, and `bay hook` answers allow when it panics or
//! runs past its deadline (its budget and a margin, well within the timeout). A deliberate deny
//! (a destructive command, a held path, a stale write) still denies, by exit 2. Main's guard
//! (merge and the host hook) judges every change. Doctor's `hooks` row names an entry an earlier
//! bay wrote with `"block"` or without the timeout, and a re-install replaces it.
//!
//! Guards to mutate: in `src/agent_settings.rs` the `PreToolUse` shape's `on_failure` (make it
//! `"block"`: the install cases fail); in `src/main.rs` `hook`'s judging thread (judge on the
//! main thread: the crashing case exits 101) and its deadline (wait for the answer with no bound:
//! the hung case never answers).

// Each case runs shell scripts (a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{TestBox, result_json, row};

const CLAUDE_ADAPTER: &str = "main = \"main\"\n\n[agent]\nproduct = \"claude\"\n";

fn settings(repo: &Path) -> Value {
    let text = std::fs::read_to_string(repo.join(".claude/settings.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// bay's `bay hook tool.before` hook in the settings.
fn pre_tool_hook(settings: &Value) -> Value {
    settings["hooks"]["PreToolUse"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["hooks"].as_array().unwrap())
        .find(|hook| {
            hook["command"]
                .as_str()
                .is_some_and(|command| command.ends_with("bay hook tool.before"))
        })
        .cloned()
        .unwrap_or_else(|| panic!("no bay pre-tool hook: {settings}"))
}

fn claude_repo(test_box: &TestBox) -> std::path::PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    repo
}

/// `bay hook tool.before` for a Bash call of `command` in `repo`, with `env`: its exit code and
/// how long it took, or `None` when it had not answered within 30 s.
fn hook(
    test_box: &TestBox,
    repo: &Path,
    command: &str,
    env: &[(&str, &str)],
) -> Option<(Option<i32>, Duration)> {
    let payload = serde_json::json!({
        "session_id": "g158-session",
        "transcript_path": "/dev/null",
        "cwd": repo,
        "permission_mode": "bypassPermissions",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "tool_use_id": "toolu_g158",
    });
    let mut run = test_box.bay_process(repo);
    run.args(["hook", "tool.before"])
        .envs(env.iter().copied())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = run.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let began = Instant::now();
    while began.elapsed() < Duration::from_secs(30) {
        if let Some(status) = child.try_wait().unwrap() {
            return Some((status.code(), began.elapsed()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

#[test]
fn install_writes_continue_and_a_thirty_second_timeout_on_the_pre_tool_entry() {
    let test_box = TestBox::new();
    let repo = claude_repo(&test_box);
    test_box.bay_ok(&repo, &["install"]);
    let hook = pre_tool_hook(&settings(&repo));
    assert_eq!(hook["onFailure"], "continue", "{hook}");
    assert_eq!(hook["timeout"], 30, "{hook}");
    // Only the pre-tool entry carries them: the other events keep the product's defaults.
    let all = settings(&repo);
    let stop = &all["hooks"]["Stop"][0]["hooks"][0];
    assert!(stop.get("timeout").is_none(), "{stop}");

    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let hooks = row(&result, "hooks");
    assert_eq!(hooks["state"], "green", "{hooks}");
    assert_eq!(hooks["pre_tool_timeout_s"], 30, "{hooks}");
    assert!(hooks.get("timeout_note").is_none(), "{hooks}");
    assert!(hooks.get("on_failure_note").is_none(), "{hooks}");
}

#[test]
fn a_reinstall_replaces_an_earlier_entrys_block_and_adds_its_timeout() {
    let test_box = TestBox::new();
    let repo = claude_repo(&test_box);
    test_box.bay_ok(&repo, &["install"]);
    // The entry as an earlier bay wrote it: `"block"`, and no timeout.
    let path = repo.join(".claude/settings.json");
    let mut old = settings(&repo);
    for group in old["hooks"]["PreToolUse"].as_array_mut().unwrap() {
        for hook in group["hooks"].as_array_mut().unwrap() {
            let hook = hook.as_object_mut().unwrap();
            hook.remove("timeout");
            hook.insert("onFailure".to_owned(), Value::from("block"));
        }
    }
    std::fs::write(&path, old.to_string()).unwrap();

    // Doctor accepts the entry (the guarantee does not rest on it) and says what to change.
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let hooks = row(&result, "hooks");
    assert_eq!(hooks["state"], "green", "{hooks}");
    for note in ["timeout_note", "on_failure_note"] {
        assert!(
            hooks[note]
                .as_str()
                .unwrap_or_default()
                .contains("run install again"),
            "{note}: {hooks}"
        );
    }

    test_box.bay_ok(&repo, &["install"]);
    let hook = pre_tool_hook(&settings(&repo));
    assert_eq!(hook["onFailure"], "continue", "{hook}");
    assert_eq!(hook["timeout"], 30, "{hook}");
}

#[test]
fn a_hung_hook_lets_the_call_through_within_its_deadline() {
    let test_box = TestBox::new();
    let repo = claude_repo(&test_box);
    // A budget of 1 s: the deadline is 1 s and the margin, well within the entry's 30 s.
    let answer = hook(
        &test_box,
        &repo,
        "git reset --hard",
        &[("BAY_TEST_HOOK_HANG", "1"), ("BAY_TEST_HOOK_BUDGET_S", "1")],
    );
    let (code, took) = answer.expect("the hung hook never answered");
    assert_eq!(code, Some(0), "a hung hook did not let the call through");
    assert!(took < Duration::from_secs(15), "it answered after {took:?}");
}

#[test]
fn a_crashing_hook_lets_the_call_through() {
    let test_box = TestBox::new();
    let repo = claude_repo(&test_box);
    let answer = hook(
        &test_box,
        &repo,
        "git reset --hard",
        &[("BAY_TEST_HOOK_PANIC", "1")],
    );
    let (code, _) = answer.expect("the crashing hook never answered");
    assert_eq!(
        code,
        Some(0),
        "a crashing hook did not let the call through"
    );
}

#[test]
fn a_deliberate_deny_still_denies() {
    let test_box = TestBox::new();
    let repo = claude_repo(&test_box);
    let (code, _) = hook(&test_box, &repo, "git reset --hard", &[]).expect("no answer");
    assert_eq!(code, Some(2), "a destructive command was not denied");
    let (code, _) = hook(&test_box, &repo, "ls -la", &[]).expect("no answer");
    assert_eq!(code, Some(0));
}

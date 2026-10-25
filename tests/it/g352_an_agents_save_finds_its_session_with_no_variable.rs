//! G352 (owner ruling 2026-10-24, BACKLOG 66 C): an agent's save finds its session with no
//! variable.
//!
//! Only Claude Code hands an agent's shell its session (`BAY_SESSION`). Under any other product
//! an agent's own `bay save` knew no session, so the lock its own hook had just taken refused it
//! `PATH_HELD` until the turn ended. A product runs its hooks and its shell commands through a
//! shell from its own process, so each hook now names its session against that process (pid
//! and start time), and a bay command with no `BAY_SESSION` takes the session of the nearest
//! ancestor so named. A command outside the agent's process tree finds none, and is refused as
//! before.
//!
//! The agent here is `xargs`: a process that is no shell and runs each command through `sh`, as
//! a product does.
//!
//! Guard to mutate: `src/agent_pid.rs` `session_of_caller`: return `None` at once (the agent's
//! own save is refused `PATH_HELD` by its own lock).

// The agent's process tree is read from the system: Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use support::{TestBox, UNCHECKED_POLICY, result_json};

/// The commands an agent runs, each through `sh`, under one `xargs` process: its hook's
/// `tool.before` of a write of `art/logo.bin` as session `s-x`, the write, and `bay save`.
/// Returns the save's result.
fn an_agent_edits_and_saves(test_box: &TestBox, repo: &Path) -> Value {
    let payload = test_box.path().join("payload.json");
    std::fs::write(
        &payload,
        json!({
            "session_id": "s-x",
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": repo.join("art/logo.bin"), "content": "x"},
            "cwd": repo,
        })
        .to_string(),
    )
    .unwrap();
    let out = test_box.path().join("saved.json");
    let script = r#""$BAY" -C "$REPO" hook tool.before < "$PAYLOAD" || exit 9
printf 'logo 2\n' > "$REPO/art/logo.bin"
cd "$REPO" && "$BAY" --json save "a new logo" > "$OUT"
exit 0"#;
    // The test environment bay runs in, with no `BAY_SESSION`.
    let env = test_box.bay_process(repo);
    let mut agent = Command::new("xargs");
    agent.env_clear();
    for (key, value) in env.get_envs() {
        if let Some(value) = value {
            agent.env(key, value);
        }
    }
    let mut child = agent
        .args(["-I", "{}", "sh", "-c", script])
        .env("BAY", env!("CARGO_BIN_EXE_bay"))
        .env("REPO", repo)
        .env("PAYLOAD", &payload)
        .env("OUT", &out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"go\n").unwrap();
    let ran = child.wait_with_output().unwrap();
    assert!(
        ran.status.success(),
        "the agent's commands: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
    serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap()
}

#[test]
fn an_agents_own_save_takes_its_hooks_lock_and_a_terminals_does_not() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("art/logo.bin", "logo 1\n")],
        &[
            (".bay/policy.toml".to_owned(), UNCHECKED_POLICY.to_owned()),
            (".gitattributes".to_owned(), "art/** lockable\n".to_owned()),
        ],
    );
    let remote = test_box.path().join("repo-remote.git");
    let saved = an_agent_edits_and_saves(&test_box, &repo);
    assert_ne!(saved["reason"], "PATH_HELD", "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:art/logo.bin"]),
        "logo 2"
    );

    // The agent holds the lock still; a save from outside its process tree (a terminal) is
    // no part of its session, and is refused as before.
    let payload = json!({
        "session_id": "s-y",
        "hook_event_name": "PreToolUse",
        "tool_name": "Write",
        "tool_input": {"file_path": repo.join("art/logo.bin"), "content": "x"},
        "cwd": repo,
    });
    let mut hook = test_box
        .bay_process(&repo)
        .args(["hook", "tool.before"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let held = hook.wait_with_output().unwrap();
    assert_eq!(held.status.code(), Some(2), "s-x's lock is live");
    std::fs::write(repo.join("art/logo.bin"), "logo 3\n").unwrap();
    let (code, refused) = result_json(test_box.bay(&repo).args(["save", "another logo"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "PATH_HELD", "{refused}");
}

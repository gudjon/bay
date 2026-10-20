//! G169. A hook entry never denies on a box without bay.
//!
//! `.claude/settings.json` is tracked, so it reaches every box that checks the repository out,
//! with or without bay. Under `onFailure: "block"` a hook command that cannot start denies, so a
//! bare `bay hook pre-tool` would deny every Edit, Write and Bash on a box without bay. bay's
//! guarantee never rests on the client hook (land and the host hook judge again), and a box
//! without bay runs no bay session to guard, so the entry bay installs exits 0 when `bay` does
//! not resolve on `PATH`. Where bay resolves, the entry is bay's: a failing bay, or bay's deny,
//! still denies. A re-install replaces the bare entry an earlier bay wrote, and doctor still
//! recognises both forms as bay's.

// Each case runs shell scripts (the hook entry, a fake bay on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use support::{TestBox, result_json, row, write_executable};

const CLAUDE_ADAPTER: &str = "trunk = \"main\"\n\n[agent]\nproduct = \"claude\"\n";

fn settings(repo: &Path) -> Value {
    let text = std::fs::read_to_string(repo.join(".claude/settings.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// Every hook command under `event`.
fn commands(settings: &Value, event: &str) -> Vec<String> {
    settings["hooks"][event]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["hooks"].as_array().unwrap())
        .filter_map(|hook| hook["command"].as_str().map(str::to_owned))
        .collect()
}

/// bay's `PreToolUse` command as `bay install` wrote it.
fn pre_tool_command(repo: &Path) -> String {
    commands(&settings(repo), "PreToolUse")
        .into_iter()
        .find(|command| command.ends_with("bay hook pre-tool"))
        .expect("bay's pre-tool entry")
}

fn installed(test_box: &TestBox) -> std::path::PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    test_box.bay_ok(&repo, &["install"]);
    repo
}

/// Runs `command` the way the product runs a hook command, `sh -c`, from `repo` with `path` as
/// `PATH` and a Bash call of `shell` on stdin. Returns its exit code.
fn run_entry(
    test_box: &TestBox,
    repo: &Path,
    command: &str,
    path: &str,
    shell: &str,
) -> Option<i32> {
    let payload = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash",
        "tool_input": {"command": shell}, "cwd": repo, "session_id": "s-g169"});
    let mut child = Command::new("/bin/sh")
        .args(["-c", command])
        .current_dir(repo)
        .env_clear()
        .env("PATH", path)
        .env("HOME", &test_box.home)
        .env("GIT_CONFIG_GLOBAL", &test_box.global_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CEILING_DIRECTORIES", test_box.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The entry need not read its payload: on a box without bay it exits 0 at once, so the
    // write may meet a closed pipe. Only the exit code is the answer.
    let written = child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes());
    if let Err(error) = written {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe, "{error}");
    }
    child.wait_with_output().unwrap().status.code()
}

#[test]
fn the_entry_exits_zero_without_bay_and_is_bays_where_bay_resolves() {
    let test_box = TestBox::new();
    let repo = installed(&test_box);
    let command = pre_tool_command(&repo);

    // A box without bay: nothing named bay on PATH. The entry allows.
    let empty = test_box.path().join("no-bay-bin");
    std::fs::create_dir_all(&empty).unwrap();
    let no_bay = empty.display().to_string();
    assert_eq!(
        run_entry(&test_box, &repo, &command, &no_bay, "git reset --hard"),
        Some(0),
        "{command}"
    );

    // A bay that fails: its exit is the entry's, so the product denies.
    let failing = test_box.path().join("failing-bay-bin");
    std::fs::create_dir_all(&failing).unwrap();
    write_executable(&failing.join("bay"), "#!/bin/sh\nexit 2\n");
    assert_eq!(
        run_entry(
            &test_box,
            &repo,
            &command,
            &failing.display().to_string(),
            "ls"
        ),
        Some(2),
        "{command}"
    );

    // The real bay, as installed, and a command on its deny list: bay's deny exit.
    let real = format!(
        "{}:{}:/bin:/usr/bin",
        test_box.fakebin.display(),
        test_box.local_bin().display()
    );
    assert!(test_box.local_bin().join("bay").is_file());
    assert_eq!(
        run_entry(&test_box, &repo, &command, &real, "git reset --hard"),
        Some(2),
        "{command}"
    );
    assert_eq!(
        run_entry(&test_box, &repo, &command, &real, "git status"),
        Some(0),
        "{command}"
    );
}

#[test]
fn every_event_entry_tests_for_bay_and_keeps_its_timeout_and_on_failure() {
    let test_box = TestBox::new();
    let repo = installed(&test_box);
    let all = settings(&repo);
    for (event, bay_event) in [
        ("SessionStart", "session-start"),
        ("PreToolUse", "pre-tool"),
        ("Stop", "stop"),
        ("SessionEnd", "session-end"),
    ] {
        let wanted = format!("command -v bay >/dev/null 2>&1 || exit 0; exec bay hook {bay_event}");
        assert!(commands(&all, event).contains(&wanted), "{event}: {all}");
    }
    let hook = &all["hooks"]["PreToolUse"][0]["hooks"][0];
    assert_eq!(hook["timeout"], 30, "{hook}");
    assert_eq!(hook["onFailure"], "block", "{hook}");
}

#[test]
fn a_reinstall_replaces_the_bare_entry_an_earlier_bay_wrote() {
    let test_box = TestBox::new();
    let repo = installed(&test_box);
    // The entries as an earlier bay wrote them: bare `bay hook <event>`.
    let path = repo.join(".claude/settings.json");
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("command -v bay >/dev/null 2>&1 || exit 0; exec ", "");
    std::fs::write(&path, &text).unwrap();
    assert_eq!(
        commands(&settings(&repo), "PreToolUse"),
        ["bay hook pre-tool"]
    );

    // Doctor recognises the bare entries as bay's and names them.
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let hooks = row(&result, "hooks");
    assert_eq!(hooks["state"], "green", "{hooks}");
    assert_eq!(hooks["pre_tool_timeout_s"], 30, "{hooks}");
    assert!(
        hooks["outdated"]
            .as_array()
            .is_some_and(|events| events.contains(&json!("PreToolUse"))),
        "{hooks}"
    );

    // A re-install replaces them, one entry per event, and doctor has nothing to name.
    test_box.bay_ok(&repo, &["install"]);
    let commands = commands(&settings(&repo), "PreToolUse");
    assert_eq!(commands.len(), 1, "{commands:?}");
    assert!(commands[0].starts_with("command -v bay "), "{commands:?}");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let hooks = row(&result, "hooks");
    assert_eq!(hooks["state"], "green", "{hooks}");
    assert!(hooks.get("outdated").is_none(), "{hooks}");
}

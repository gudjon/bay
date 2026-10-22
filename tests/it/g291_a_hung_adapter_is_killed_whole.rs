//! G291. A hung adapter is killed whole: past its bound, no child it started outlives it.
//!
//! bay runs every adapter one way (`crate::run`): in a process group of its own, killed whole
//! at its bound. A wake adapter and an executor `submit` adapter each start a child
//! (`sleep 300`), write the child's pid to a file, and hang waiting on it. After the bound the
//! talk's ack records the wake as failed and the land fails with `submission_timeout`, as
//! before; and the child is gone too, by `kill -0` on its pid.
//!
//! The guards to mutate, both in `src/run.rs`:
//!
//! - `cmd.process_group(0);` in `bounded`: drop it, and the adapter shares bay's group, the
//!   group kill misses it, and its child outlives the bound;
//! - `kill_group(group);` in `kill`: drop it, and only the adapter itself is killed, so its
//!   child outlives the bound.

// Each case runs shell scripts (the adapters) and `kill`, so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::talk::{TalkCase, from_now, hosted_talk_seeded};
use support::{TestBox, check_files, passing_result, result_json};

/// An adapter that reads its stdin, starts a child that outlives any bound, writes the child's
/// pid to `$HOME/../<pid_file>`, and hangs waiting on it.
fn hanging(pid_file: &str) -> String {
    format!("#!/bin/sh\ncat >/dev/null\nsleep 300 &\necho $! > \"$HOME/../{pid_file}\"\nwait\n")
}

/// Asserts the process whose pid is in `pid_file` is gone within a few seconds, and kills it
/// when it is not, so a failing run leaves nothing behind.
fn assert_gone(pid_file: &Path, what: &str) {
    let pid = std::fs::read_to_string(pid_file)
        .unwrap_or_else(|error| panic!("{what}: no pid file {}: {error}", pid_file.display()));
    let pid = pid.trim().to_owned();
    assert!(!pid.is_empty(), "{what}: the pid file is empty");
    let alive = || {
        Command::new("kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    // The killed child is reaped by its new parent, which may take a moment.
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let survived = alive();
    if survived {
        let _ = Command::new("kill").args(["-KILL", &pid]).status();
    }
    assert!(
        !survived,
        "{what}: the adapter's child {pid} outlived the bound"
    );
}

fn wake_of(status: &Value, id: &str) -> Value {
    status["detail"]["talks"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["talk"] == id))
        .map_or(Value::Null, |row| row["wake"].clone())
}

#[test]
fn a_hung_wake_adapter_and_its_child_are_killed_at_the_bound() {
    let wake = hanging("wake-child.pid");
    let case: TalkCase = hosted_talk_seeded(|test_box, a| {
        let script = a.join("scripts/bay-wake");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, &wake).unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        test_box.seed_trunk(
            a,
            &[
                ("src/a.rs", "fn a() {}\n"),
                (
                    ".bay/config.toml",
                    "[plugin.wake]\ncommand = \"scripts/bay-wake\"\n",
                ),
                ("scripts/bay-wake", wake.as_str()),
            ],
        );
    });
    let follow_up = from_now(3600);
    let (code, sent) = case.talk(
        "agent",
        &[
            "agent-b",
            "Rebase onto the new parser",
            "--kind",
            "delivery",
            "--change",
            "abcd2345",
            "--follow-up",
            &follow_up,
        ],
    );
    assert_eq!(code, 0, "{sent}");
    let id = sent["detail"]["talk"].as_str().unwrap().to_owned();
    let started = Instant::now();
    let (_, accepted) = case.status("agent-b");
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        wake_of(&accepted, &id),
        serde_json::json!({"result": "uncertain", "reason": "WAKE_ADAPTER_FAILED"}),
        "{accepted}"
    );
    assert_gone(&case.test_box.home.join("../wake-child.pid"), "wake");
}

#[test]
fn a_hung_executor_submit_and_its_child_are_killed_at_the_bound() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push(("scripts/executor".to_owned(), hanging("submit-child.pid")));
    files.push((
        ".bay/config.toml".to_owned(),
        "[runner]\ncommand = \"scripts/executor\"\nsubmission_timeout_s = 1\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    std::fs::set_permissions(
        repo.join("scripts/executor"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, failed) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 20, "{failed}");
    assert!(
        failed.to_string().contains("submission timeout"),
        "{failed}"
    );
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let op = &status["detail"]["changes"][0]["operation"];
    assert_eq!(op["outcome"], "submission_timeout", "{status}");
    assert_gone(
        &test_box.home.join("../submit-child.pid"),
        "executor submit",
    );
}

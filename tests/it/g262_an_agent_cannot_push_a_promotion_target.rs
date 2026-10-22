//! G262 (NEW-5). An agent cannot push a promotion target.
//!
//! A `[[target]]` (`staging`) moves only by bay's promotion rules, never by an agent's push.
//! `bay hook tool.before` reads the targets of the config at trunk and denies, with
//! `SHELL_DENIED` and the shape `git push of a target`, a push whose destination is a target's
//! ref, in every refspec form: the full ref, the short name, `HEAD:`, `+`, `--force`,
//! `--delete`, and a bare push from the target's branch. The reason names the target's ref. A
//! push of an ordinary branch still gets the allow exit.
//!
//! The guard is the target check in `denied_push` (`src/deny.rs`): make its
//! `.find(|reference| pushes_target(reference.as_str()))` find nothing and this test fails.

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use support::TestBox;

/// The config at trunk: the `staging` target, promoted from the trunk.
const STAGING: &str = "trunk = \"main\"\n\n[[target]]\nname = \"staging\"\n";

fn pre_tool(test_box: &TestBox, repo: &Path, command: &str) -> (Option<i32>, String) {
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": repo,
    });
    let mut child = test_box
        .bay_process(repo)
        .args(["hook", "tool.before"])
        .stdin(Stdio::piped())
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

/// A repository whose trunk carries the `staging` target in its config.
fn with_target(test_box: &TestBox) -> PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), (".bay/config.toml", STAGING)],
    );
    repo
}

fn assert_denied_naming_the_target(test_box: &TestBox, repo: &Path, command: &str) {
    let (code, reason) = pre_tool(test_box, repo, command);
    assert_eq!(code, Some(2), "{command} was allowed");
    assert!(reason.contains("bay denies"), "{command}: {reason}");
    assert!(
        reason.contains("promotion target refs/heads/staging"),
        "{command} does not name the target: {reason}"
    );
}

#[test]
fn every_refspec_form_of_a_push_to_a_target_is_denied_naming_it() {
    let test_box = TestBox::new();
    let repo = with_target(&test_box);
    for command in [
        "git push origin x:refs/heads/staging",
        "git push origin x:staging",
        "git push origin staging",
        "git push origin HEAD:staging",
        "git push origin +x:staging",
        "git push --force origin x:staging",
        "git push origin --delete staging",
        "bash -c 'git push origin HEAD:staging'",
    ] {
        assert_denied_naming_the_target(&test_box, &repo, command);
    }
}

#[test]
fn a_bare_push_from_the_targets_branch_is_denied_naming_it() {
    let test_box = TestBox::new();
    let repo = with_target(&test_box);
    test_box.git_ok(&repo, &["checkout", "-q", "-b", "staging"]);
    for command in ["git push", "git push origin", "git push origin HEAD"] {
        assert_denied_naming_the_target(&test_box, &repo, command);
    }
}

#[test]
fn a_push_of_an_ordinary_branch_gets_the_allow_exit() {
    let test_box = TestBox::new();
    let repo = with_target(&test_box);
    for command in [
        "git push origin topic",
        "git push origin x:topic",
        "git push origin HEAD:staging-notes",
        "git push origin refs/bay/changes/abcd2345",
    ] {
        let (code, reason) = pre_tool(&test_box, &repo, command);
        assert_eq!(code, Some(0), "{command}: {reason}");
    }
}

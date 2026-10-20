//! G152. An unnamed actor is visible, and refusable.
//!
//! With `BAY_ACTOR` unset bay names the actor by the user name, else `agent`, so on an account
//! that many sessions share every session is one actor. The change record, the results that
//! name an actor and every session row say where the name came from (`actor_source`: `env`,
//! `user-default` or `default`); `bay status --fleet` counts the sessions whose actor came from
//! a default; and a policy at the base with `[agent] require_actor = true` makes `bay start`
//! refuse `ACTOR_UNNAMED` while `BAY_ACTOR` is unset.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use support::{TestBox, UNCHECKED_POLICY, result_json};

fn hook(test_box: &TestBox, cwd: &Path, payload: &Value, envs: &[(&str, &str)]) {
    let mut command = test_box.bay_process(cwd);
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command
        .args(["hook", "session-start"])
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
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(0));
}

fn record(test_box: &TestBox, repo: &Path, id: &str) -> Value {
    serde_json::from_str(&test_box.git_ok(
        repo,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    ))
    .unwrap()
}

#[test]
fn start_says_where_the_actor_name_came_from() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);

    for (intent, envs, name, source) in [
        ("CARD-1", vec![("BAY_ACTOR", "named")], "named", "env"),
        (
            "CARD-2",
            vec![("USER", "someone")],
            "someone",
            "user-default",
        ),
        ("CARD-3", vec![], "agent", "default"),
    ] {
        let mut command = test_box.bay(&repo);
        for (key, value) in &envs {
            command.env(key, value);
        }
        let (code, result) = result_json(command.args(["start", intent, "--paths", "src/"]));
        assert_eq!(code, 0, "{result}");
        assert_eq!(result["detail"]["actor"]["name"], name, "{result}");
        assert_eq!(result["detail"]["actor_source"], source, "{result}");
        let id = result["change"].as_str().unwrap();
        let record = record(&test_box, &repo, id);
        assert_eq!(record["actor_source"], source, "{record}");
    }
}

#[test]
fn session_rows_carry_the_source_and_the_fleet_counts_the_defaults() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    test_box.git_ok(&repo, &["config", "bay.machine", "box-a"]);

    let session =
        |id: &str| json!({"session_id": id, "hook_event_name": "SessionStart", "cwd": repo});
    hook(
        &test_box,
        &repo,
        &session("s-named"),
        &[("BAY_ACTOR", "named")],
    );
    hook(&test_box, &repo, &session("s-user"), &[("USER", "someone")]);
    hook(&test_box, &repo, &session("s-bare"), &[]);
    for (id, source) in [
        ("s-named", "env"),
        ("s-user", "user-default"),
        ("s-bare", "default"),
    ] {
        let path = repo.join(format!(".git/bay/sessions/{id}.jsonl"));
        let text = std::fs::read_to_string(&path).unwrap();
        let line: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(line["actor_source"], source, "{line}");
    }

    test_box.bay_ok(&repo, &["status"]);
    let (code, result) = result_json(test_box.bay(&repo).args(["status", "--fleet"]));
    assert_eq!(code, 0, "{result}");
    let detail = &result["detail"];
    assert_eq!(detail["default_actor_sessions"], 2, "{result}");
    let sessions = detail["sessions"].as_array().unwrap();
    let named = sessions
        .iter()
        .find(|row| row["session"] == "s-named")
        .unwrap_or_else(|| panic!("{result}"));
    assert_eq!(named["actor_source"], "env", "{named}");
}

#[test]
fn a_policy_that_requires_a_named_actor_refuses_an_unnamed_start() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let policy = format!("{UNCHECKED_POLICY}\n[agent]\nrequire_actor = true\n");
    test_box.seed_trunk(
        &repo,
        &[("src/a.rs", "fn a() {}\n"), (".bay/policy.toml", &policy)],
    );

    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .env("USER", "someone")
            .args(["start", "CARD-1", "--paths", "src/"]),
    );
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "ACTOR_UNNAMED", "{result}");
    assert_eq!(result["detail"]["actor"], "someone", "{result}");
    assert_eq!(result["detail"]["actor_source"], "user-default", "{result}");
    assert!(
        result["detail"]["recovery"]
            .as_str()
            .is_some_and(|recovery| recovery.contains("BAY_ACTOR")),
        "{result}"
    );
    // Nothing was claimed: the refusal comes before the change.
    let claims = test_box.git_ok(&repo, &["for-each-ref", "refs/bay/"]);
    assert!(!claims.contains("refs/bay/changes/"), "{claims}");

    let (code, result) = result_json(
        test_box
            .bay(&repo)
            .env("BAY_ACTOR", "named")
            .args(["start", "CARD-1", "--paths", "src/"]),
    );
    assert_eq!(code, 0, "{result}");
    assert_eq!(result["detail"]["actor_source"], "env", "{result}");
}

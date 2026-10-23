//! G243b. A policy can require proof of which actor signs.
//!
//! G243: when several actors run sessions under one OS account, which can read a file-based
//! agent key, any of them can sign as any other, and doctor's `key-custody` row says
//! `actor_proof: "account"`. A policy at the base with `[agent] require_actor_proof = true` makes
//! `bay merge` refuse `ACTOR_UNPROVEN` there, before anything is signed, naming a hardware agent
//! key or one OS account per actor. A hardware agent key proves the actor, so it lands. Without
//! the key in the policy nothing is refused. The key is a fake file and the session records fake
//! lines; doctor and land read only paths, modes, owners and public halves.

// Each case runs shell scripts (a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::json;
use support::{TestBox, UNCHECKED_POLICY, result_json, row};

const REQUIRE: &str = "\n[agent]\nrequire_actor_proof = true\n";

/// A repository whose trunk holds `src/a.rs` and an unchecked policy (with `extra` appended), a
/// file-based agent key configured, and session records of two actors under this OS account.
/// Returns (box, repo, remote, key).
fn shared_account(extra: &str) -> (TestBox, PathBuf, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let policy = format!("{UNCHECKED_POLICY}{extra}");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[(".bay/policy.toml".to_owned(), policy)],
    );
    let dir = test_box.path().join("keys");
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join("agent");
    std::fs::write(&key, "-----BEGIN OPENSSH PRIVATE KEY-----\nNOT-A-KEY\n").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    test_box.git_ok(&repo, &["config", "bay.agentKey", key.to_str().unwrap()]);
    sessions(&repo, &[("s-1", "actor-a"), ("s-2", "actor-b")]);
    (test_box, repo, remote, key)
}

/// Session records of this machine, written as this OS user in the shape the session hook
/// writes: the actor is a name and a session.
fn sessions(repo: &Path, records: &[(&str, &str)]) {
    let dir = repo.join(".git/bay/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    for (session, actor) in records {
        std::fs::write(
            dir.join(format!("{session}.jsonl")),
            format!(
                "{{\"schema\":\"bay.session/1\",\"session\":\"{session}\",\"actor\":{{\"name\":\"{actor}\",\"session\":\"{session}\"}},\"event\":\"session-start\"}}\n"
            ),
        )
        .unwrap();
    }
}

fn custody(test_box: &TestBox, repo: &Path) -> serde_json::Value {
    let (_, result) = result_json(test_box.bay(repo).arg("doctor"));
    row(&result, "key-custody").clone()
}

#[test]
fn a_shared_account_that_can_read_the_agent_key_is_refused_actor_unproven() {
    let (test_box, repo, remote, _) = shared_account(REQUIRE);
    let trunk = test_box.git_bare(&remote, &["rev-parse", "main"]);
    let listed = custody(&test_box, &repo);
    assert_eq!(listed["actor_proof"], "account", "{listed}");
    assert_eq!(listed["require_actor_proof"], true, "{listed}");
    assert!(listed["actor_refusal"].is_string(), "{listed}");

    let (id, view) = test_box.start_and_edit(&repo);
    let (code, result) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["decision"], "blocked", "{result}");
    assert_eq!(result["reason"], "ACTOR_UNPROVEN", "{result}");
    assert_eq!(result["change"], id, "{result}");
    assert_eq!(result["allowed"], json!(["read"]), "{result}");
    assert_eq!(result["retry_when"], "after_setup", "{result}");
    let detail = &result["detail"];
    assert_eq!(detail["actor_proof"], "account", "{result}");
    assert_eq!(detail["actors"], json!(["actor-a", "actor-b"]), "{result}");
    assert_eq!(detail["policy"], ".bay/policy.toml", "{result}");
    let recovery = detail["recovery"].as_str().unwrap();
    assert!(
        recovery.contains("hardware agent key") && recovery.contains("own OS account"),
        "{recovery}"
    );
    // Refused before anything was requested: trunk did not move, and no operation was written.
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), trunk);
    let status = test_box.bay_ok(&repo, &["status", &id]);
    let change = &status["detail"]["changes"][0];
    assert_ne!(change["state"], "landed", "{status}");
}

#[test]
fn a_hardware_agent_key_proves_the_actor_and_lands() {
    let (test_box, repo, remote, key) = shared_account(REQUIRE);
    // A hardware key's file is a handle: it signs only with a touch on the device.
    std::fs::write(
        key.with_extension("pub"),
        "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIDgOPZsmzgIilFlX+FEXSGHrntiIAzexLc6I0f+I9/4qAAAAD3NzaDpiYXktcmVsZWFzZQ== agent\n",
    )
    .unwrap();
    let listed = custody(&test_box, &repo);
    assert_eq!(listed["actor_proof"], "key", "{listed}");
    assert_eq!(listed["require_actor_proof"], true, "{listed}");
    assert!(listed.get("actor_refusal").is_none(), "{listed}");

    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

#[test]
fn without_the_policy_key_a_shared_account_lands() {
    let (test_box, repo, remote, _) = shared_account("");
    let listed = custody(&test_box, &repo);
    assert_eq!(listed["actor_proof"], "account", "{listed}");
    assert_eq!(listed["require_actor_proof"], false, "{listed}");
    assert!(listed.get("actor_refusal").is_none(), "{listed}");

    let (id, view) = test_box.start_and_edit(&repo);
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id]);
}

//! G145. Doctor names a signing key that the account running agent sessions can read.
//!
//! `DESIGN` § 10a (key classes). A file-based release, producer, executor or registry private
//! key, or a token file beside it, that this OS user can read is red with `KEY_EXPOSED` when
//! this machine's session records show agent sessions under this user, or when the user has
//! passwordless sudo (`sudo -n true`). Doctor checks the path, mode and ownership only, never
//! the key's contents. The keys here are fake files, the session record a fake line, and
//! `sudo` a fake on PATH.

// Each case runs shell scripts (a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use support::{TestBox, result_json, row};

/// A marker inside each fake key: doctor never prints it.
const SECRET: &str = "NOT-A-KEY-SECRET-MARKER";

/// A fake private key file at `<root>/keys/<name>`, mode 0600, with a token file beside it.
fn fake_key(test_box: &TestBox, name: &str) -> PathBuf {
    let dir = test_box.path().join("keys");
    std::fs::create_dir_all(&dir).unwrap();
    let key = dir.join(name);
    std::fs::write(
        &key,
        format!("-----BEGIN OPENSSH PRIVATE KEY-----\n{SECRET}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let token = dir.join("gh-token");
    std::fs::write(&token, SECRET).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    key
}

/// A session record of this machine, written as this OS user, as an agent hook writes one.
fn session_record(repo: &Path) {
    let dir = repo.join(".git/bay/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("s-1.jsonl"),
        "{\"schema\":\"bay.session/1\",\"session\":\"s-1\",\"event\":\"session-start\"}\n",
    )
    .unwrap();
}

fn sudo_calls(test_box: &TestBox) -> String {
    std::fs::read_to_string(test_box.path().join("sudo.log")).unwrap_or_default()
}

#[test]
fn a_producer_key_readable_where_agent_sessions_run_is_red() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let key = fake_key(&test_box, "producer");
    test_box.git_ok(&repo, &["config", "bay.producerKey", key.to_str().unwrap()]);
    session_record(&repo);

    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "KEY_EXPOSED", "{result}");
    let custody = row(&result, "key-custody");
    assert_eq!(custody["state"], "red", "{custody}");
    assert_eq!(custody["session_records"], 1, "{custody}");
    let listed = &custody["keys"][0];
    assert_eq!(listed["setting"], "bay.producerKey", "{custody}");
    assert_eq!(listed["class"], "producer", "{custody}");
    assert_eq!(listed["path"], key.to_str().unwrap(), "{custody}");
    assert_eq!(listed["mode"], "0600", "{custody}");
    assert_eq!(listed["readable"], true, "{custody}");
    let token = test_box.path().join("keys/gh-token");
    assert_eq!(listed["tokens"][0], token.to_str().unwrap(), "{custody}");
    let note = custody["note"].as_str().unwrap();
    assert!(note.contains("agent sessions"), "{note}");
    let recovery = custody["recovery"].as_str().unwrap();
    assert!(
        recovery.contains("ed25519-sk") && recovery.contains("outside this account"),
        "{recovery}"
    );
    assert!(!result.to_string().contains(SECRET), "{result}");
}

/// G75's reviewer key is a signing key like the others: readable where agent sessions run, it
/// is red (the review dossier's correction K7).
#[test]
fn a_reviewer_key_readable_where_agent_sessions_run_is_red() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let key = fake_key(&test_box, "reviewer");
    test_box.git_ok(&repo, &["config", "bay.reviewerKey", key.to_str().unwrap()]);
    session_record(&repo);
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "KEY_EXPOSED", "{result}");
    let custody = row(&result, "key-custody");
    assert_eq!(
        custody["keys"][0]["setting"], "bay.reviewerKey",
        "{custody}"
    );
    assert_eq!(custody["keys"][0]["class"], "reviewer", "{custody}");
}

#[test]
fn a_release_key_readable_by_a_user_with_passwordless_sudo_is_red() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let key = fake_key(&test_box, "release");
    test_box.git_ok(&repo, &["config", "bay.releaseKey", key.to_str().unwrap()]);

    // No passwordless sudo and no session records here: the key is listed, green.
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let custody = row(&result, "key-custody");
    assert_eq!(custody["state"], "green", "{custody}");
    assert_eq!(custody["passwordless_sudo"], false, "{custody}");
    assert_eq!(custody["keys"][0]["class"], "release", "{custody}");
    assert_eq!(sudo_calls(&test_box).trim(), "-n true");

    test_box.fake_sudo(true);
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "KEY_EXPOSED", "{result}");
    let custody = row(&result, "key-custody");
    assert_eq!(custody["state"], "red", "{custody}");
    assert_eq!(custody["passwordless_sudo"], true, "{custody}");
    assert!(
        custody["note"]
            .as_str()
            .unwrap()
            .contains("passwordless sudo"),
        "{custody}"
    );
}

#[test]
fn the_gate_producer_key_is_named_outside_a_repository() {
    let test_box = TestBox::new();
    let key = fake_key(&test_box, "gate");
    test_box.git_ok(
        test_box.path(),
        &[
            "config",
            "--global",
            "bay.gateProducerKey",
            key.to_str().unwrap(),
        ],
    );
    test_box.fake_sudo(true);
    let outside = test_box.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();

    let (_, result) = result_json(test_box.bay(&outside).arg("doctor"));
    let custody = row(&result, "key-custody");
    assert_eq!(custody["state"], "red", "{custody}");
    assert_eq!(
        custody["keys"][0]["setting"], "bay.gateProducerKey",
        "{custody}"
    );
}

#[test]
fn a_hardware_key_an_agent_key_or_no_key_is_green() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    session_record(&repo);
    test_box.fake_sudo(true);

    // No key configured: nothing to name, and sudo is not asked.
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "key-custody")["state"], "green", "{result}");
    assert_eq!(sudo_calls(&test_box), "");

    // An agent key belongs to the session account by design.
    let agent = fake_key(&test_box, "agent");
    test_box.git_ok(&repo, &["config", "bay.agentKey", agent.to_str().unwrap()]);
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "key-custody")["state"], "green", "{result}");

    // A hardware key's file is a handle: it signs only with a touch on the device.
    let executor = fake_key(&test_box, "executor");
    std::fs::write(
        executor.with_extension("pub"),
        "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIDgOPZsmzgIilFlX+FEXSGHrntiIAzexLc6I0f+I9/4qAAAAD3NzaDpiYXktcmVsZWFzZQ== exec\n",
    )
    .unwrap();
    // A token beside it is still red whatever the key is, so this case has none.
    std::fs::remove_file(test_box.path().join("keys/gh-token")).unwrap();
    test_box.git_ok(
        &repo,
        &["config", "bay.runnerKey", executor.to_str().unwrap()],
    );
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let custody = row(&result, "key-custody");
    assert_eq!(custody["state"], "green", "{custody}");
    let listed = custody["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|key| key["setting"] == "bay.runnerKey")
        .unwrap();
    assert_eq!(listed["hardware"], true, "{custody}");
}

/// G243: two actors run sessions under this account, which can read the agent key: bay cannot
/// prove which actor signed, and says so. Advisory: the box is not refused. One actor, a hardware
/// agent key, or no agent key at all leaves the row green.
#[test]
fn actors_sharing_an_os_account_are_named_and_unproven() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let dir = repo.join(".git/bay/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    // The shape the session hook writes: the actor is a name and a session.
    for (session, actor) in [("s-1", "actor-a"), ("s-2", "actor-b"), ("s-3", "actor-a")] {
        std::fs::write(
            dir.join(format!("{session}.jsonl")),
            format!(
                "{{\"schema\":\"bay.session/1\",\"session\":\"{session}\",\"actor\":{{\"name\":\"{actor}\",\"session\":\"{session}\"}},\"event\":\"session-start\"}}\n"
            ),
        )
        .unwrap();
    }
    let doctor = || {
        let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
        assert_eq!(code, 0, "{result}");
        assert_ne!(result["reason"], "KEY_EXPOSED", "{result}");
        row(&result, "key-custody").clone()
    };

    // Two actors but no agent key: there is nothing to sign as another actor with.
    let custody = doctor();
    assert_eq!(custody["state"], "green", "{custody}");
    assert_eq!(custody["actor_proof"], "none", "{custody}");

    let key = fake_key(&test_box, "agent");
    test_box.git_ok(&repo, &["config", "bay.agentKey", key.to_str().unwrap()]);
    let custody = doctor();
    assert_eq!(custody["actor_proof"], "account", "{custody}");
    assert_eq!(
        custody["actors"],
        serde_json::json!(["actor-a", "actor-b"]),
        "{custody}"
    );
    assert_eq!(custody["state"], "degraded", "{custody}");
    assert!(custody["actor_note"].is_string(), "{custody}");

    // A hardware agent key signs only with a touch on its device: the key proves who signed.
    std::fs::write(
        key.with_extension("pub"),
        "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIDgOPZsmzgIilFlX+FEXSGHrntiIAzexLc6I0f+I9/4qAAAAD3NzaDpiYXktcmVsZWFzZQ== agent\n",
    )
    .unwrap();
    let custody = doctor();
    assert_eq!(custody["state"], "green", "{custody}");
    assert_eq!(custody["actor_proof"], "key", "{custody}");
    std::fs::remove_file(key.with_extension("pub")).unwrap();

    // One actor, in two sessions: the key proves who signed.
    std::fs::remove_file(dir.join("s-2.jsonl")).unwrap();
    let custody = doctor();
    assert_eq!(custody["state"], "green", "{custody}");
    assert_eq!(custody["actor_proof"], "key", "{custody}");
    assert!(custody.get("actor_note").is_none(), "{custody}");
}

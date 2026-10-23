//! G339 (owner ruling 2026-10-23, BACKLOG 53): the runner's key class is `runner`, and what an
//! earlier bay stored under `executor` carries over.
//!
//! `bay enrol runner` enrols a runner key; `executor` is no class name any more. A registry an
//! earlier bay signed holds the runner keys in its `executor` file: bay reads it as the runner
//! class, so its keys keep verifying, and the next enrol of a runner key writes them all into
//! `runner` and drops `executor`. The Git config keys are `bay.runnerKey`, `bay.runnerName` and
//! `bay.runnerSandbox`: `bay install` moves an old `bay.executor*` value to the new name, and
//! `bay doctor` names an old one still set.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/registry.rs` `read_files`: read only `class.file()` (the old file is not read: the
//!   runner key is not enrolled).
//! - `src/enrol.rs` `new_tree`: drop the `OLD_RUNNER_FILE` entry push (the enrol keeps
//!   `executor` beside `runner`).
//! - `src/install.rs` `run`: drop the `carry_over` call (the old key stays, the new one unset).
//! - `src/doctor.rs` `probe`: drop the `renamed-keys` row.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::{EXECUTOR, REGISTRAR, TestBox, key_line, result_json, unchecked_files};

fn row<'a>(doctor: &'a Value, name: &str) -> &'a Value {
    doctor["detail"]["rows"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == name))
        .unwrap_or_else(|| panic!("no {name} row: {doctor}"))
}

fn tree_names(test_box: &TestBox, repo: &Path) -> Vec<String> {
    test_box
        .git_ok(repo, &["ls-tree", "--name-only", "refs/bay/registry"])
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_registry_stored_with_an_executor_file_keeps_its_runner_keys() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let keys = test_box.keys();
    // As a bay before 0.4.0 signed it: the runner key in `executor`.
    test_box.enroll_files(
        &repo,
        &keys,
        &[
            ("executor", key_line(EXECUTOR, &keys.executor)),
            ("registry", key_line(REGISTRAR, &keys.registry)),
        ],
    );
    test_box.use_keys(&repo, &keys);
    let (_, doctor) = result_json(test_box.bay(&repo).args(["doctor", "--role", "runner"]));
    assert_eq!(
        row(&doctor, "runner-keys")["runner"],
        "enrolled",
        "{doctor}"
    );

    // The next enrol of a runner key writes `runner` with both keys and drops `executor`.
    test_box.git_ok(
        &repo,
        &["config", "bay.registryKey", keys.registry.to_str().unwrap()],
    );
    let second = keys.agent.with_extension("pub");
    let enrolled = test_box.bay_ok(
        &repo,
        &["enrol", "runner", "exec-2", second.to_str().unwrap()],
    );
    assert_eq!(enrolled["detail"]["class"], "runner", "{enrolled}");
    let names = tree_names(&test_box, &repo);
    assert!(!names.contains(&"executor".to_owned()), "{names:?}");
    let runners = test_box.git_ok(&repo, &["show", "refs/bay/registry:runner"]);
    assert!(
        runners.contains(&format!("{EXECUTOR} ")) && runners.contains("exec-2 "),
        "{runners}"
    );
    let (_, doctor) = result_json(test_box.bay(&repo).args(["doctor", "--role", "runner"]));
    assert_eq!(
        row(&doctor, "runner-keys")["runner"],
        "enrolled",
        "{doctor}"
    );

    // `executor` is no class name: a usage error.
    let refused = test_box
        .bay(&repo)
        .args(["enrol", "executor", "exec-3", second.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
}

#[test]
fn install_carries_the_old_key_names_over_and_doctor_names_one_left() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    test_box.git_ok(&repo, &["config", "bay.executorKey", "/keys/runner"]);
    test_box.git_ok(&repo, &["config", "bay.executorName", "r1"]);
    test_box.git_ok(
        &repo,
        &["config", "--global", "bay.executorSandbox", "none"],
    );
    let doctor = test_box.bay_ok(&repo, &["doctor"]);
    let keys = row(&doctor, "renamed-keys")["keys"].to_string();
    assert!(
        keys.contains("bay.executorKey") && keys.contains("bay.executorSandbox"),
        "{doctor}"
    );

    let installed = test_box.bay_ok(&repo, &["install"]);
    assert_eq!(
        test_box.git_ok(&repo, &["config", "--local", "bay.runnerKey"]),
        "/keys/runner",
        "{installed}"
    );
    assert_eq!(
        test_box.git_ok(&repo, &["config", "--local", "bay.runnerName"]),
        "r1"
    );
    assert!(
        test_box
            .git(&repo)
            .args(["config", "--local", "bay.executorKey"])
            .output()
            .unwrap()
            .stdout
            .is_empty(),
        "the old key stayed"
    );
    // A clone's install leaves global config alone; the machine install moves it.
    let doctor = test_box.bay_ok(&repo, &["doctor"]);
    let keys = row(&doctor, "renamed-keys")["keys"].to_string();
    assert!(
        keys.contains("bay.executorSandbox") && !keys.contains("bay.executorKey"),
        "{doctor}"
    );
    test_box.bay_ok(&repo, &["install", "--machine"]);
    assert_eq!(
        test_box.git_ok(&repo, &["config", "--global", "bay.runnerSandbox"]),
        "none"
    );
    let doctor = test_box.bay_ok(&repo, &["doctor"]);
    let rows = doctor["detail"]["rows"].as_array().unwrap();
    assert!(
        !rows.iter().any(|row| row["name"] == "renamed-keys"),
        "{doctor}"
    );
}

/// A bay host takes a registry whose `removed` file names the runner class `executor`, as a
/// bay before 0.4.0 wrote it, and a later enrol, which keeps that removal, passes it too.
///
/// Guard to mutate: `src/registry.rs` `KeyClass::named`: drop the `OLD_RUNNER_FILE` arm (the
/// old removal line names no class, so the host refuses the registry as unreadable).
#[test]
fn a_bay_host_keeps_a_removal_stored_under_the_old_class_name() {
    use support::AGENT;
    use support::approval::{APPROVER, trunk_enrolled_by};
    let public = |key: &std::path::Path| key.with_extension("pub").to_str().unwrap().to_owned();
    let trunk = trunk_enrolled_by("", "", |test_box, repo, keys, approver_key| {
        test_box.git_ok(
            repo,
            &["config", "bay.registryKey", keys.registry.to_str().unwrap()],
        );
        for (class, name, key) in [
            ("registry", REGISTRAR, keys.registry.as_path()),
            ("agent", AGENT, keys.agent.as_path()),
            ("approver", APPROVER, approver_key),
        ] {
            test_box.bay_ok(repo, &["enrol", class, name, &public(key)]);
        }
    });
    let (test_box, repo) = (&trunk.test_box, &trunk.repo);
    let keys = test_box.keys();
    test_box.git_ok(
        repo,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/registry:refs/bay/registry",
        ],
    );
    // The registry as it stands, plus a removal written the way a bay before 0.4.0 wrote it.
    let mut files: Vec<(String, String)> = test_box
        .git_ok(repo, &["ls-tree", "--name-only", "refs/bay/registry"])
        .lines()
        .map(|name| {
            let text = test_box.git_ok(repo, &["show", &format!("refs/bay/registry:{name}")]);
            (name.to_owned(), format!("{text}\n"))
        })
        .collect();
    let old = key_line(EXECUTOR, &keys.executor);
    let mut words = old.split_whitespace().skip(1);
    let (key_type, blob) = (words.next().unwrap(), words.next().unwrap());
    files.push((
        "removed".to_owned(),
        format!("executor {EXECUTOR} rotated 2026-10-01T00:00:00Z {key_type} {blob}\n"),
    ));
    let files: Vec<(&str, String)> = files
        .iter()
        .map(|(name, text)| (name.as_str(), text.clone()))
        .collect();
    let commit = test_box.registry_commit(repo, &keys, &files);
    let pushed = test_box
        .git(repo)
        .args(["push", "origin", &format!("{commit}:refs/bay/registry")])
        .output()
        .unwrap();
    assert!(pushed.status.success(), "{pushed:?}");
    test_box.git_ok(repo, &["update-ref", "refs/bay/registry", &commit]);

    // A later enrol keeps that removal, and the host takes it.
    let fresh = test_box.path().join("keys/agent-b");
    let made = std::process::Command::new("ssh-keygen")
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-f",
            fresh.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(made.status.success(), "{made:?}");
    let (code, enrolled) =
        result_json(
            test_box
                .bay(repo)
                .args(["enrol", "agent", "agent-b", &public(&fresh)]),
        );
    assert_eq!(code, 0, "the host refused the enrol: {enrolled}");
    let removed = test_box.git_ok(repo, &["show", "refs/bay/registry:removed"]);
    assert!(
        removed.starts_with(&format!("executor {EXECUTOR} rotated ")),
        "{removed}"
    );
}

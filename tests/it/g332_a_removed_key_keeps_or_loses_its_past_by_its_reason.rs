//! G332 (owner ruling 2026-10-22, de-enrolment): a removed key keeps or loses its past by the
//! reason of its removal.
//!
//! `bay enrol --remove <class> <name> --reason rotated|revoked` takes the name's keys out of
//! the class file and records each in the registry's `removed` file. A record a rotated key
//! signed before its removal still counts, judged at its signing time: `bay evidence missing`
//! does not list the merge it covers. A revoked key's records count nowhere: the merge is listed
//! again, its record unverifiable. The last registry key is never removed.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/evidence.rs` `verifies_in`: `class_file_at(git, signer_class(&self.kind), &self.at)`
//!   to `class_file(git, signer_class(&self.kind))` (a rotated key's record no longer counts).
//! - `src/registry.rs` `class_file_at`: `removal.reason == Removed::Rotated` to `true` (a
//!   revoked key's record still counts).
//! - `src/enrol.rs` `remove`: drop the last-registry-key refusal (the registry loses its signer).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use serde_json::Value;
use support::{PRODUCER, TestBox, TestKeys, result_json};

/// A repository with a registry and a merge on main covered by one signed `check` record of
/// producer `ci-1`; the repository, the keys and the merge.
fn covered() -> (TestBox, PathBuf, TestKeys, String) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_trunk(&repo, &[("src/a.rs", "fn a() {}\n")]);
    // A commit with a parent: a record is about a diff on its base.
    std::fs::write(repo.join("src/b.rs"), "fn b() {}\n").unwrap();
    test_box.git_ok(&repo, &["add", "src/b.rs"]);
    test_box.git_ok(&repo, &["commit", "-q", "-m", "b"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);
    let tip = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    let keys = test_box.keys();
    test_box.enroll(&repo, &keys);
    test_box.use_keys(&repo, &keys);
    test_box.git_ok(
        &repo,
        &["config", "bay.registryKey", keys.registry.to_str().unwrap()],
    );
    let recorded = test_box.bay_ok(
        &repo,
        &[
            "evidence",
            "record",
            "--kind",
            "check",
            "--verdict",
            "pass",
            "--commit",
            &tip,
        ],
    );
    assert_eq!(recorded["decision"], "ok", "{recorded}");
    assert!(
        missing(&test_box, &repo).is_empty(),
        "the record covers the merge before any removal"
    );
    // A removal at a later second than the record's signing time.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    (test_box, repo, keys, tip)
}

/// The rows `bay evidence missing --kind check --producer ci-1` lists for the merge on main:
/// `--since` its parent, so the first commit, which no record covers, is not asked about.
fn missing(test_box: &TestBox, repo: &std::path::Path) -> Vec<Value> {
    let result = test_box.bay_ok(
        repo,
        &[
            "evidence",
            "missing",
            "--kind",
            "check",
            "--producer",
            PRODUCER,
            "--since",
            "HEAD~1",
        ],
    );
    result["detail"]["missing"].as_array().unwrap().clone()
}

fn remove(
    test_box: &TestBox,
    repo: &std::path::Path,
    class: &str,
    name: &str,
    reason: &str,
) -> (i32, Value) {
    result_json(
        test_box
            .bay(repo)
            .args(["enrol", "--remove", class, name, "--reason", reason]),
    )
}

#[test]
fn a_rotated_keys_past_records_still_count() {
    let (test_box, repo, _keys, _tip) = covered();
    let (code, removed) = remove(&test_box, &repo, "producer", PRODUCER, "rotated");
    assert_eq!(code, 0, "{removed}");
    assert_eq!(removed["detail"]["removed"], "rotated", "{removed}");
    assert_eq!(removed["detail"]["keys"], 1, "{removed}");
    let removed_file = test_box.git_ok(&repo, &["show", "refs/bay/registry:removed"]);
    assert!(
        removed_file.starts_with(&format!("producer {PRODUCER} rotated ")),
        "{removed_file}"
    );
    let rows = missing(&test_box, &repo);
    assert!(
        rows.is_empty(),
        "a rotated key's record stopped counting: {rows:?}"
    );
}

#[test]
fn a_revoked_keys_records_count_nowhere() {
    let (test_box, repo, _keys, tip) = covered();
    let (code, removed) = remove(&test_box, &repo, "producer", PRODUCER, "revoked");
    assert_eq!(code, 0, "{removed}");
    let rows = missing(&test_box, &repo);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["commit"], tip.as_str(), "{rows:?}");
    assert_eq!(rows[0]["trust"], "unverifiable", "{rows:?}");
}

#[test]
fn the_last_registry_key_is_never_removed() {
    let (test_box, repo, _keys, _tip) = covered();
    let before = test_box.git_ok(&repo, &["rev-parse", "refs/bay/registry"]);
    let (code, refused) = remove(&test_box, &repo, "registry", support::REGISTRAR, "rotated");
    assert_eq!(code, 20, "{refused}");
    assert!(
        refused["detail"]
            .as_str()
            .is_some_and(|why| why.contains("last registry key")),
        "{refused}"
    );
    assert_eq!(
        test_box.git_ok(&repo, &["rev-parse", "refs/bay/registry"]),
        before,
        "the registry moved"
    );
}

/// The removal passes a bay host's hook, and a registry version that drops it again, even one
/// the registry key signs, is refused there.
///
/// Guard to mutate: `src/hook.rs` `Judge::registry`: drop the dropped-removal refusal (the
/// version that undoes the removal is taken).
#[test]
fn a_bay_host_takes_a_removal_and_refuses_its_undoing() {
    use support::approval::{APPROVER, trunk_enrolled_by};
    use support::{AGENT, REGISTRAR, key_line};
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
    let (code, removed) = remove(test_box, repo, "approver", APPROVER, "revoked");
    assert_eq!(code, 0, "the host refused the removal: {removed}");

    // Signed by the registry key, on the version that removed the approver, with no removed file.
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
    let undone = test_box.registry_commit(
        repo,
        &keys,
        &[
            ("registry", key_line(REGISTRAR, &keys.registry)),
            ("agent", key_line(AGENT, &keys.agent)),
        ],
    );
    let out = test_box
        .git(repo)
        .args(["push", "origin", &format!("{undone}:refs/bay/registry")])
        .output()
        .unwrap();
    assert!(!out.status.success(), "the host took the undoing: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("never undone"),
        "{out:?}"
    );
}

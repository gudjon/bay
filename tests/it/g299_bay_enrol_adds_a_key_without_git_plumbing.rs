//! G299: `bay enrol` adds a key without Git plumbing (owner ruling 2026-10-21).
//!
//! `bay enrol <class> <name> <public-key-file>` adds one key to the key registry. It reads the
//! registry as the remote holds it, appends the key's line to its class file, keeps every other
//! file, checks the new registry by the rules a registry is loaded by, signs it with this
//! machine's registry key (`bay.registryKey`) on the version before it, and pushes it as a
//! fast-forward the host's hook accepts. The first key enrolled is that registry key. An
//! approver enrolled this way approves, and the change lands. Each refusal names its reason and
//! next step, and nothing moves on the host: a key already enrolled (refused before anything is
//! signed or pushed), a first key of another class or of another key than `bay.registryKey`'s,
//! and a machine with no `bay.registryKey`.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/enrol.rs` `enrol_in`: drop the `Registry::check_files(...)` call and its
//!   `.map_err(...)?;` (the duplicate is pushed and the host refuses it: the refusal no longer
//!   starts `Nothing was enrolled`).
//! - `src/enrol.rs` `enrol_in`: `None if class != KeyClass::Registry =>` to `None if false =>`
//!   (the first approver key is refused for another reason, not naming that there is no
//!   registry yet).
//! - `src/enrol.rs` `enrol_in`: `None if public != signer_public =>` to `None if false =>` (a
//!   first registry key that is not `bay.registryKey`'s is enrolled).
//! - `src/enrol.rs` `new_tree`: `.filter(|entry| !entry.is_empty() && !entry.ends_with(&own))`
//!   to `.filter(|_| false)` (each version holds only its own class file: the registrar is
//!   dropped, and the next enrolment is refused).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::approval::{APPROVER, Trunk, trunk_enrolled_by};
use support::{AGENT, REGISTRAR, TestBox, TestKeys, key_line, result_json};

/// The public half of `key`, as a path argument.
fn public(key: &Path) -> String {
    key.with_extension("pub").to_str().unwrap().to_owned()
}

/// Sets `bay.registryKey` in `repo` to the test registry key.
fn set_key(test_box: &TestBox, repo: &Path, keys: &TestKeys) {
    test_box.git_ok(
        repo,
        &["config", "bay.registryKey", keys.registry.to_str().unwrap()],
    );
}

/// `bay enrol <class> <name> <key>.pub` in `repo`, which must decide `ok`; the new registry
/// version it names.
fn enrol_ok(test_box: &TestBox, repo: &Path, class: &str, name: &str, key: &Path) -> String {
    let result = test_box.bay_ok(repo, &["enrol", class, name, &public(key)]);
    assert_eq!(result["decision"], "ok", "{result}");
    assert_eq!(result["detail"]["class"], class, "{result}");
    assert_eq!(result["detail"]["name"], name, "{result}");
    result["detail"]["registry"].as_str().unwrap().to_owned()
}

/// `bay enrol <class> <name> <key>.pub` in the work repository, which must fail; its detail.
fn enrol_refused(trunk: &Trunk, class: &str, name: &str, key: &Path) -> String {
    let (code, refused) = result_json(trunk.test_box.bay(&trunk.repo).args([
        "enrol",
        class,
        name,
        public(key).as_str(),
    ]));
    assert_eq!(code, 20, "{refused}");
    assert_eq!(refused["decision"], "failed", "{refused}");
    refused["detail"].as_str().unwrap().to_owned()
}

fn host(trunk: &Trunk) -> PathBuf {
    trunk.test_box.path().join("host.git")
}

/// The registry version the host holds, or empty for none.
fn host_registry(trunk: &Trunk) -> String {
    trunk.test_box.git_bare(
        &host(trunk),
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/bay/registry",
        ],
    )
}

/// Approver `approver-a`'s key, made by the approval trunk.
fn approver_key(trunk: &Trunk) -> PathBuf {
    trunk.test_box.path().join("keys").join(APPROVER)
}

#[test]
fn enrol_creates_the_registry_and_an_enrolled_approver_approves() {
    let mut enrolled = Vec::new();
    let trunk = trunk_enrolled_by("", "", |test_box, repo, keys, approver_key| {
        set_key(test_box, repo, keys);
        enrolled.push(enrol_ok(
            test_box,
            repo,
            "registry",
            REGISTRAR,
            &keys.registry,
        ));
        enrolled.push(enrol_ok(test_box, repo, "agent", AGENT, &keys.agent));
        enrolled.push(enrol_ok(test_box, repo, "approver", APPROVER, approver_key));
    });
    let keys = trunk.test_box.keys();
    let host = host(&trunk);

    // The host's hook took each version, each on the one before it.
    assert_eq!(host_registry(&trunk), enrolled[2]);
    let chain = trunk
        .test_box
        .git_bare(&host, &["rev-list", "--reverse", "refs/bay/registry"]);
    assert_eq!(chain.lines().collect::<Vec<_>>(), enrolled);

    // Each class file holds its line, and every earlier file is kept.
    let file = |name: &str| {
        trunk.test_box.git_bare(
            &host,
            &["cat-file", "-p", &format!("refs/bay/registry:{name}")],
        )
    };
    assert_eq!(
        file("approver"),
        key_line(APPROVER, &approver_key(&trunk)).trim_end()
    );
    assert_eq!(file("agent"), key_line(AGENT, &keys.agent).trim_end());
    assert_eq!(
        file("registry"),
        key_line(REGISTRAR, &keys.registry).trim_end()
    );

    // The last version is signed by the registry key the version before it enrols.
    let allowed = trunk.test_box.path().join("registrars.allowed");
    std::fs::write(&allowed, key_line(REGISTRAR, &keys.registry)).unwrap();
    let git_dir = format!("--git-dir={}", host.display());
    let setting = format!("gpg.ssh.allowedSignersFile={}", allowed.display());
    let verified = trunk
        .test_box
        .git(trunk.test_box.path())
        .args([
            git_dir.as_str(),
            "-c",
            setting.as_str(),
            "verify-commit",
            enrolled[2].as_str(),
        ])
        .output()
        .unwrap();
    assert!(
        verified.status.success(),
        "{}",
        String::from_utf8_lossy(&verified.stderr)
    );

    // The approver enrolled by `bay enrol` approves from their own clone, and the change lands.
    let (id, view, _) = trunk.held_change();
    let (code, approved) = trunk.approve(&trunk.approver_box, &id, &[]);
    assert_eq!(code, 0, "{approved}");
    assert_eq!(approved["detail"]["approver"], APPROVER, "{approved}");
    let (code, landed) = trunk.land(&view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
}

#[test]
fn a_key_already_enrolled_is_refused_and_nothing_moves() {
    let trunk = trunk_enrolled_by("", "", |test_box, repo, keys, approver_key| {
        set_key(test_box, repo, keys);
        enrol_ok(test_box, repo, "registry", REGISTRAR, &keys.registry);
        enrol_ok(test_box, repo, "approver", APPROVER, approver_key);
    });
    let before = host_registry(&trunk);
    assert_ne!(before, "", "the host holds no registry");

    // The same key under a second approver name, and as a reviewer: one key, one place.
    for class in ["approver", "reviewer"] {
        let why = enrol_refused(&trunk, class, "approver-c", &approver_key(&trunk));
        assert!(why.starts_with("Nothing was enrolled"), "{why}");
        assert!(why.contains("this key is already in"), "{why}");
        assert_eq!(host_registry(&trunk), before, "the host's registry moved");
    }
    let local = trunk
        .test_box
        .git_ok(&trunk.repo, &["rev-parse", "refs/bay/registry"]);
    assert_eq!(local, before, "this repository's registry moved");
}

#[test]
fn a_first_key_that_is_not_this_machines_registry_key_is_refused() {
    let trunk = trunk_enrolled_by("", "", |test_box, repo, keys, _| {
        set_key(test_box, repo, keys);
    });
    let keys = trunk.test_box.keys();

    let why = enrol_refused(&trunk, "approver", APPROVER, &approver_key(&trunk));
    assert!(why.contains("no registry yet"), "{why}");
    assert!(why.contains("bay enrol registry"), "{why}");
    assert_eq!(host_registry(&trunk), "", "a registry was created");

    // A registry key, but not the one `bay.registryKey` signs with.
    let why = enrol_refused(&trunk, "registry", REGISTRAR, &keys.rogue);
    assert!(why.contains("bay.registryKey"), "{why}");
    assert!(why.contains("bay enrol registry"), "{why}");
    assert_eq!(host_registry(&trunk), "", "a registry was created");
}

#[test]
fn no_registry_key_is_refused_naming_the_config_line() {
    let trunk = trunk_enrolled_by("", "", |_, _, _, _| {});
    let keys = trunk.test_box.keys();
    let why = enrol_refused(&trunk, "registry", REGISTRAR, &keys.registry);
    assert!(why.contains("git config bay.registryKey"), "{why}");
    assert_eq!(host_registry(&trunk), "", "a registry was created");
}

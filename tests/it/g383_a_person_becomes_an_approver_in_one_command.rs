//! G383 (owner ruling 2026-10-25: "bay setup for a non-technical person must be ONE command";
//! the night's real attempt pasted seven lines, and its registry never reached the remote). A
//! person becomes an approver in one command.
//!
//! `bay install --approver <name>`, in the project's clone, makes the registry and approver keys
//! in `~/.ssh` when there are none (mode 0600, no prompt), sets them in the clone, starts the
//! registry under `[policy] signing = "approvals"`, enrols the approver, and reads both back from
//! the remote, saying one plain sentence. A second run changes nothing. Under `signing = "all"`
//! it starts no registry, which would make every landing need a runner key, and says why.
//!
//! Guards to mutate, in `src/setup.rs`: the signing test (start a registry under "all": the
//! second case fails); the `holds` test before the approver's enrolment (enrol it again on a
//! second run, which `bay enrol` refuses: the first case fails). Each enrolment's own push and
//! read-back is `bay enrol`'s, held by G299.

#![cfg(unix)]

use crate::support;

use std::os::unix::fs::PermissionsExt as _;

use support::{TestBox, UNCHECKED_POLICY, result_json};

fn project(policy_extra: &str) -> (TestBox, std::path::PathBuf, std::path::PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[(
            ".bay/policy.toml".to_owned(),
            format!("{UNCHECKED_POLICY}{policy_extra}"),
        )],
    );
    (test_box, repo, remote)
}

#[test]
fn one_run_makes_the_keys_enrols_and_reads_back_and_a_second_changes_nothing() {
    let (test_box, repo, remote) = project("\n[policy]\nsigning = \"approvals\"\n");
    let (code, done) = result_json(test_box.bay(&repo).args(["install", "--approver", "ada"]));
    assert_eq!(code, 0, "{done}");
    assert!(
        done["say"]
            .as_str()
            .is_some_and(|say| say.starts_with("Done: you can now approve")),
        "{done}"
    );
    assert_eq!(done["detail"]["started_registry"], true, "{done}");
    assert_eq!(done["detail"]["enrolled"], true, "{done}");
    let key = test_box.home.join(".ssh/bay-approver-ada");
    let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the approver key is private");
    let on_remote = test_box.git_bare(&remote, &["rev-parse", "refs/bay/registry"]);
    assert_eq!(done["detail"]["registry"], on_remote.as_str(), "{done}");
    let approvers = test_box.git_bare(&remote, &["ls-tree", "--name-only", "refs/bay/registry"]);
    assert!(approvers.contains("approver"), "{approvers}");

    let (code, again) = result_json(test_box.bay(&repo).args(["install", "--approver", "ada"]));
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["detail"]["enrolled"], false, "{again}");
    assert_eq!(again["detail"]["started_registry"], false, "{again}");
    assert_eq!(
        test_box.git_bare(&remote, &["rev-parse", "refs/bay/registry"]),
        on_remote,
        "a second run moved the registry"
    );
    assert!(
        again["say"]
            .as_str()
            .is_some_and(|say| say.starts_with("Already set up")),
        "{again}"
    );
}

#[test]
fn under_signing_all_no_registry_is_started() {
    let (test_box, repo, remote) = project("");
    let (code, refused) = result_json(test_box.bay(&repo).args(["install", "--approver", "ada"]));
    assert_ne!(code, 0, "{refused}");
    assert!(
        refused.to_string().contains("signing = \\\"approvals\\\""),
        "{refused}"
    );
    let listed = test_box.git_bare(&remote, &["for-each-ref", "refs/bay/registry"]);
    assert!(
        listed.is_empty(),
        "a registry was started under signing = all: {listed}"
    );
}

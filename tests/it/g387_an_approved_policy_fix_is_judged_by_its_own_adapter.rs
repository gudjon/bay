//! G387 (owner ruling 2026-10-25, "approval = repair"): under `[policy] signing = "approvals"`
//! a change of only the policy and its adapters, which wakes an approval check of the base, is
//! judged by its own new adapter, and still waits on the base's approval.
//!
//! Under `signing = "approvals"` no request is signed, so the registry-signed repair of
//! `signing = "all"` never exists there, and the base's broken adapter used to fail the very
//! change that fixes it. Now the fix runs its own adapter, and the base's approval check is still
//! owed, even when the fix drops it from its own policy: a person's yes stands in for the
//! repair's signature, and nothing lands without it.
//!
//! Guard to mutate, in `src/publish.rs`: `approval_repair` returning `Ok(None)` (the base's
//! broken adapter fails the fix), or its `retain` keeping no check (the fix lands unapproved).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::talk::fresh_key;
use support::{AGENT, REGISTRAR, TestBox, check_files, key_line, passing_result, result_json};

const APPROVER: &str = "approver-a";
const CHECK: &str = "rules-approval";
const APPROVAL: &str = "\n[[check]]\nid = \"rules-approval\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\".bay/\", \"scripts/\"]\n";
const APPROVALS_ONLY: &str = "\n[policy]\nsigning = \"approvals\"\n";

#[test]
fn a_policy_fix_runs_its_own_adapter_and_waits_on_the_bases_approval() {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    // The base's adapter is broken: it ends before it says anything.
    let mut base = check_files("exit 3", &passing_result(1), 1, 60);
    for (path, text) in &mut base {
        if path == ".bay/policy.toml" {
            text.push_str(APPROVAL);
            text.push_str(APPROVALS_ONLY);
        }
    }
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &base);
    let keys = test_box.keys();
    let approver_key = fresh_key(&test_box, APPROVER);
    test_box.enroll_files(
        &repo,
        &keys,
        &[
            ("agent", key_line(AGENT, &keys.agent)),
            ("approver", key_line(APPROVER, &approver_key)),
            ("registry", key_line(REGISTRAR, &keys.registry)),
        ],
    );
    let approver_box = test_box.clone_of(&host, "approver-box");
    test_box.git_ok(&approver_box, &["config", "bay.approverName", APPROVER]);
    test_box.git_ok(
        &approver_box,
        &["config", "bay.approverKey", approver_key.to_str().unwrap()],
    );

    // The fix: a working adapter, the revision raised, and its own policy without the approval.
    let started = test_box.bay_ok(
        &repo,
        &[
            "start", "CARD-fix", "--paths", ".bay/", "--paths", "scripts/",
        ],
    );
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    for (path, text) in check_files("true", &passing_result(1), 1, 60) {
        let text = if path == ".bay/policy.toml" {
            format!(
                "{}{APPROVALS_ONLY}",
                text.replace("revision = 1", "revision = 2")
            )
        } else {
            text
        };
        std::fs::write(view.join(&path), text).unwrap();
    }
    let (code, waiting) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{waiting}");
    assert_eq!(waiting["reason"], "APPROVAL_PENDING", "{waiting}");
    assert_eq!(
        waiting["detail"]["approval"]["checks"][0]["check"], CHECK,
        "{waiting}"
    );
    let main = |dir| test_box.git_ok(dir, &["rev-parse", "refs/remotes/origin/main"]);
    let before = main(&repo);

    let status = test_box.bay_ok(&approver_box, &["status", &id]);
    let candidate = status["detail"]["changes"][0]["operation"]["approval"]["commit"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, approved) = result_json(test_box.bay(&approver_box).args([
        "evidence",
        "record",
        "--kind",
        "approval",
        "--check",
        CHECK,
        "--verdict",
        "pass",
        "--commit",
        &candidate,
    ]));
    assert_eq!(code, 0, "{approved}");
    let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_ne!(main(&repo), before, "trunk moved");
}

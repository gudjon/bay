//! G288: an approval counts the same everywhere (One place, one truth; G245).
//!
//! Without a key registry no approver key can be verified, so no `approval` record counts. That
//! rule has one home, `evidence::tagged` (read through `evidence::counted`), and every
//! reader takes it from there: landing, a `[[target]]`'s promotion rule in `bay status`
//! (`detail.targets`) and in the host hook. A target that requires an approval check is not
//! promotable on a trunk commit that holds only an approval no registry verifies, and a change
//! that owes the same approval stays `APPROVAL_PENDING`.
//!
//! The guard to mutate: in `evidence::tagged`, the branch without a registry that names an
//! `approval` record `Verified::Unverifiable`. Make it `Verified::Unsigned` and the target
//! becomes promotable, the hook takes the push, and the held change lands.

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::talk::fresh_key;
use support::{TestBox, check_files, passing_result, result_json};

const APPROVER: &str = "approver-a";
const CHECK: &str = "billing-approval";
const APPROVAL: &str = "\n[[check]]\nid = \"billing-approval\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\"billing/\"]\n";
/// The `staging` target, requiring the approval check.
const STAGING: &str =
    "main = \"main\"\n\n[[target]]\nname = \"staging\"\nrequires = [\"billing-approval\"]\n";

/// Starts a change in `repo` under `intent` declaring `dir` and writes `path` in its view.
fn start(
    test_box: &TestBox,
    repo: &Path,
    intent: &str,
    dir: &str,
    path: &str,
) -> (String, PathBuf) {
    let started = test_box.bay_ok(repo, &["start", intent, "--paths", dir]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join(path), format!("{path}\n")).unwrap();
    (id, view)
}

/// `bay merge --wait` in `view`.
fn land(test_box: &TestBox, view: &Path) -> (i32, Value) {
    result_json(test_box.bay(view).args(["merge", "--wait"]))
}

/// `bay evidence record` of a passing approval about `commit`, from `dir`.
fn approve(test_box: &TestBox, dir: &Path, commit: &str) {
    let (code, written) = result_json(test_box.bay(dir).args([
        "evidence",
        "record",
        "--kind",
        "approval",
        "--check",
        CHECK,
        "--verdict",
        "pass",
        "--commit",
        commit,
    ]));
    assert_eq!(code, 0, "{written}");
}

/// The `detail.targets` row of `staging` in `bay status` in `dir`.
fn staging(test_box: &TestBox, dir: &Path) -> Value {
    let status = test_box.bay_ok(dir, &["status"]);
    status["detail"]["targets"]
        .as_array()
        .unwrap_or_else(|| panic!("no detail.targets: {status}"))
        .iter()
        .find(|row| row["name"] == "staging")
        .unwrap_or_else(|| panic!("no target staging: {status}"))
        .clone()
}

#[test]
fn without_a_registry_an_approval_promotes_nothing_and_lands_nothing() {
    // A bay host with no key registry: its hook still judges promotion targets.
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str(APPROVAL);
        }
    }
    files.push((".bay/config.toml".to_owned(), STAGING.to_owned()));
    test_box.seed_with_check(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("billing/rates.txt", "rate 1\n"),
        ],
        &files,
    );

    // A landing elsewhere owes no approval: it is the trunk commit a promotion would name.
    let (_, view) = start(&test_box, &repo, "CARD-src", "src/", "src/b.rs");
    let (code, landed) = land(&test_box, &view);
    assert_eq!(code, 0, "{landed}");
    let trunk_commit = landed["detail"]["commit"].as_str().unwrap().to_owned();

    // A change to an approval path is held for its approval.
    let (id, view) = start(
        &test_box,
        &repo,
        "CARD-billing",
        "billing/",
        "billing/discount.txt",
    );
    let (code, held) = land(&test_box, &view);
    assert_eq!(code, 10, "{held}");
    assert_eq!(held["reason"], "APPROVAL_PENDING", "{held}");
    let candidate = held["detail"]["approval"]["commit"]
        .as_str()
        .unwrap()
        .to_owned();

    // The approver's box: a name and a key, which no registry enrols.
    let approver_box = test_box.clone_of(&host, "approver-box");
    let approver_key = fresh_key(&test_box, APPROVER);
    test_box.git_ok(&approver_box, &["config", "bay.approverName", APPROVER]);
    test_box.git_ok(
        &approver_box,
        &["config", "bay.approverKey", approver_key.to_str().unwrap()],
    );
    test_box.bay_ok(&approver_box, &["status", &id]);
    approve(&test_box, &approver_box, &candidate);
    approve(&test_box, &approver_box, &trunk_commit);

    // Landing: the approval does not count, and the change is still held.
    let (code, still) = land(&test_box, &view);
    assert_eq!(
        code, 10,
        "an unverified approval landed the change: {still}"
    );
    assert_eq!(still["reason"], "APPROVAL_PENDING", "{still}");

    // Status: the target is not promotable, for the same approval check, still pending.
    let row = staging(&test_box, &approver_box);
    assert_eq!(row["promotable"]["commit"], Value::Null, "{row}");
    let blocked = &row["promotable"]["blocked_by"][0];
    assert_eq!(blocked["rule"], "check", "{row}");
    assert_eq!(blocked["check"], CHECK, "{row}");
    assert_eq!(blocked["state"], "pending", "{row}");

    // The hook: the push that would promote the trunk commit is refused, naming the check.
    let out = test_box
        .git(&approver_box)
        .args([
            "push",
            "origin",
            &format!("{trunk_commit}:refs/heads/staging"),
        ])
        .output()
        .unwrap();
    let refused = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !out.status.success(),
        "the host promoted on an unverified approval: {refused}"
    );
    assert!(refused.contains("PROMOTION_REFUSED"), "{refused}");
    assert!(
        refused.contains(&format!("check {CHECK} is pending")),
        "{refused}"
    );
}

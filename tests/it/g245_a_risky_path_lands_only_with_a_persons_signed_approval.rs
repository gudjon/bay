//! G245: a risky path lands only with a person's signed approval (owner ruling 2026-10-20).
//!
//! A `[[check]] kind = "approval"` with `when_paths` is answered by records, never run. A
//! candidate whose diff touches its paths is held `APPROVAL_PENDING`, naming who can approve,
//! until an `approval` record for that exact candidate tree, signed by a key the registry's
//! approver file enrols, is on the remote. An approval signed by an agent key never counts, and
//! the host refuses it. A change elsewhere lands without one. A rejection blocks the change as
//! a failed check that names the approver. Without a registry no approval can count, and the
//! refusal says so.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use bay::git::Git;
use bay::registry::Registry;
use support::talk::fresh_key;
use support::{
    AGENT, EXECUTOR, PRODUCER, REGISTRAR, TestBox, TestKeys, check_files, key_line, passing_result,
    result_json,
};

const APPROVER: &str = "approver-a";
const CHECK: &str = "billing-approval";
const APPROVAL: &str = "\n[[check]]\nid = \"billing-approval\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\"billing/\"]\n";

struct Trunk {
    test_box: TestBox,
    repo: PathBuf,
    keys: TestKeys,
    /// The approver's own clone, with `bay.approverName` and `bay.approverKey`.
    approver_box: PathBuf,
    approver_key: PathBuf,
}

/// The registry files: the test registry, with `approvers` as the whole approver file.
fn files(keys: &TestKeys, approvers: &str) -> Vec<(&'static str, String)> {
    vec![
        ("agent", key_line(AGENT, &keys.agent)),
        ("runner", key_line(EXECUTOR, &keys.executor)),
        ("producer", key_line(PRODUCER, &keys.producer)),
        ("approver", approvers.to_owned()),
        ("registry", key_line(REGISTRAR, &keys.registry)),
    ]
}

/// A trunk whose policy runs `tests` on every change and asks an approval for `billing/`: on a
/// bay host with a registry that enrols approver `approver-a` when `signed`, else on a plain
/// bare remote with no registry.
fn trunk(signed: bool) -> Trunk {
    let test_box = TestBox::new();
    let (remote, repo) = if signed {
        let host = test_box.path().join("host.git");
        test_box.host_init(&host);
        let repo = test_box.repo_without_remote_any("repo");
        test_box.add_origin(&repo, &host);
        (host, repo)
    } else {
        let repo = test_box.repo_any("repo");
        (test_box.path().join("repo-remote.git"), repo)
    };
    let mut check = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut check {
        if path == ".bay/policy.toml" {
            text.push_str(APPROVAL);
        }
    }
    test_box.seed_with_check(
        &repo,
        &[
            ("src/a.rs", "fn a() {}\n"),
            ("billing/rates.txt", "rate 1\n"),
        ],
        &check,
    );
    let keys = test_box.keys();
    let approver_key = fresh_key(&test_box, APPROVER);
    if signed {
        test_box.enroll_files(
            &repo,
            &keys,
            &files(&keys, &key_line(APPROVER, &approver_key)),
        );
        test_box.use_keys(&repo, &keys);
    }
    let approver_box = test_box.clone_of(&remote, "approver-box");
    test_box.git_ok(&approver_box, &["config", "bay.approverName", APPROVER]);
    test_box.git_ok(
        &approver_box,
        &["config", "bay.approverKey", approver_key.to_str().unwrap()],
    );
    Trunk {
        test_box,
        repo,
        keys,
        approver_box,
        approver_key,
    }
}

impl Trunk {
    /// Starts a change under `intent` writing `path`. Returns the change id and its view.
    fn start(&self, intent: &str, path: &str) -> (String, PathBuf) {
        let dir = format!("{}/", Path::new(path).parent().unwrap().display());
        let started = self
            .test_box
            .bay_ok(&self.repo, &["start", intent, "--paths", &dir]);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(path), format!("{path}\n")).unwrap();
        (id, view)
    }

    fn land(&self, view: &Path) -> (i32, serde_json::Value) {
        result_json(self.test_box.bay(view).args(["merge", "--wait"]))
    }

    /// `bay evidence record` of an approval with `verdict` about candidate `commit`, from `dir`.
    fn approve(&self, dir: &Path, verdict: &str, commit: &str) -> (i32, serde_json::Value) {
        result_json(self.test_box.bay(dir).args([
            "evidence",
            "record",
            "--kind",
            "approval",
            "--check",
            CHECK,
            "--verdict",
            verdict,
            "--commit",
            commit,
        ]))
    }
}

/// The approval detail of a refusal held for `billing-approval`: the candidate commit it names.
fn held(result: &serde_json::Value, state: &str) -> String {
    let approval = &result["detail"]["approval"];
    assert_eq!(approval["checks"][0]["check"], CHECK, "{result}");
    assert_eq!(approval["checks"][0]["environment"], "people", "{result}");
    assert_eq!(approval["checks"][0]["state"], state, "{result}");
    approval["commit"].as_str().unwrap().to_owned()
}

#[test]
fn a_change_to_an_approval_path_lands_only_with_an_enrolled_approvers_record() {
    let trunk = trunk(true);
    let (id, view) = trunk.start("CARD-billing", "billing/discount.txt");

    // Every check bay runs passed; the approval is owed, and the refusal names who can give it.
    let (code, waiting) = trunk.land(&view);
    assert_eq!(code, 10, "{waiting}");
    assert_eq!(waiting["reason"], "APPROVAL_PENDING", "{waiting}");
    assert_eq!(waiting["allowed"], serde_json::json!(["wait"]), "{waiting}");
    assert_eq!(waiting["retry_when"], "after_person", "{waiting}");
    assert_eq!(
        waiting["detail"]["approval"]["checks"][0]["approvers"],
        serde_json::json!([APPROVER]),
        "{waiting}"
    );
    let candidate = held(&waiting, "pending");
    let trunk_before = trunk
        .test_box
        .git_ok(&trunk.repo, &["rev-parse", "origin/main"]);

    // `bay status` shows the same: who can approve, and the candidate to name.
    let status = trunk.test_box.bay_ok(&trunk.repo, &["status", &id]);
    let operation = &status["detail"]["changes"][0]["operation"];
    assert_eq!(operation["outcome"], "APPROVAL_PENDING", "{status}");
    assert_eq!(
        operation["approval"]["checks"][0]["approvers"],
        serde_json::json!([APPROVER]),
        "{status}"
    );
    assert_eq!(
        operation["approval"]["commit"],
        candidate.as_str(),
        "{status}"
    );

    // An approval signed by the agent's own key: the host refuses it, and where it is held
    // locally it never counts.
    trunk
        .test_box
        .git_ok(&trunk.repo, &["config", "bay.approverName", AGENT]);
    trunk.test_box.git_ok(
        &trunk.repo,
        &[
            "config",
            "bay.approverKey",
            trunk.keys.agent.to_str().unwrap(),
        ],
    );
    let (code, forged) = trunk.approve(&trunk.repo, "pass", &candidate);
    assert_ne!(code, 0, "an agent-signed approval was pushed: {forged}");
    assert!(forged.to_string().contains("not pushed"), "{forged}");
    for key in ["bay.approverName", "bay.approverKey"] {
        trunk
            .test_box
            .git_ok(&trunk.repo, &["config", "--unset", key]);
    }
    let (code, still) = trunk.land(&view);
    assert_eq!(code, 10, "{still}");
    assert_eq!(still["reason"], "APPROVAL_PENDING", "{still}");
    assert_eq!(
        trunk
            .test_box
            .git_ok(&trunk.repo, &["rev-parse", "origin/main"]),
        trunk_before,
        "trunk did not move"
    );

    // A `check` pass for the approval check, signed by the executor's producer key: the host
    // takes it (a producer signs checks), and it never answers an approval.
    let (code, written) = result_json(trunk.test_box.bay(&trunk.repo).args([
        "evidence",
        "record",
        "--kind",
        "check",
        "--check",
        CHECK,
        "--environment",
        "people",
        "--verdict",
        "pass",
        "--commit",
        &candidate,
    ]));
    assert_eq!(code, 0, "{written}");
    let (code, still) = trunk.land(&view);
    assert_eq!(code, 10, "{still}");
    assert_eq!(still["reason"], "APPROVAL_PENDING", "{still}");
    let wrong_kind = still["detail"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["producer"] == PRODUCER && row["why"] == "wrong_kind");
    assert!(wrong_kind, "{still}");

    // The approver reads the change from its own clone, approves that exact candidate, and the
    // agent's retry lands it.
    let status = trunk.test_box.bay_ok(&trunk.approver_box, &["status", &id]);
    let operation = &status["detail"]["changes"][0]["operation"];
    assert_eq!(
        operation["approval"]["checks"][0]["approvers"],
        serde_json::json!([APPROVER]),
        "{status}"
    );
    let named = operation["approval"]["commit"].as_str().unwrap().to_owned();
    let (code, approved) = trunk.approve(&trunk.approver_box, "pass", &named);
    assert_eq!(code, 0, "{approved}");
    let (code, landed) = trunk.land(&view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");

    // A change elsewhere needs no approval.
    let (_, view) = trunk.start("CARD-src", "src/other.rs");
    let (code, elsewhere) = trunk.land(&view);
    assert_eq!(code, 0, "{elsewhere}");
    assert_eq!(elsewhere["decision"], "merged", "{elsewhere}");
}

#[test]
fn a_rejection_blocks_the_change_and_names_the_approver() {
    let trunk = trunk(true);
    let (id, view) = trunk.start("CARD-billing", "billing/discount.txt");
    let (code, waiting) = trunk.land(&view);
    assert_eq!(code, 10, "{waiting}");
    assert_eq!(waiting["reason"], "APPROVAL_PENDING", "{waiting}");
    let candidate = held(&waiting, "pending");

    trunk.test_box.bay_ok(&trunk.approver_box, &["status", &id]);
    let (code, rejected) = trunk.approve(&trunk.approver_box, "product_failure", &candidate);
    assert_eq!(code, 0, "{rejected}");
    let (code, refused) = trunk.land(&view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    let named = refused["detail"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["check"] == CHECK && row["approver"] == APPROVER);
    assert!(named, "the rejection names its approver: {refused}");
    held(&refused, "rejected");
}

#[test]
fn without_a_registry_no_approval_counts_and_the_refusal_says_so() {
    let trunk = trunk(false);
    let (id, view) = trunk.start("CARD-billing", "billing/discount.txt");
    let (code, waiting) = trunk.land(&view);
    assert_eq!(code, 10, "{waiting}");
    assert_eq!(waiting["reason"], "APPROVAL_PENDING", "{waiting}");
    let candidate = held(&waiting, "pending");
    let why = waiting["detail"]["approval"]["why"]
        .as_str()
        .unwrap_or_default();
    assert!(why.contains("no key registry"), "{waiting}");

    trunk.test_box.bay_ok(&trunk.approver_box, &["status", &id]);
    let (code, approved) = trunk.approve(&trunk.approver_box, "pass", &candidate);
    assert_eq!(code, 0, "{approved}");
    let (code, still) = trunk.land(&view);
    assert_eq!(code, 10, "an unsigned approval landed the change: {still}");
    assert_eq!(still["reason"], "APPROVAL_PENDING", "{still}");
}

#[test]
fn an_approver_name_is_never_an_agent_or_a_producer_name() {
    let trunk = trunk(true);
    let git = Git::new(&trunk.repo);
    let other = fresh_key(&trunk.test_box, "other");
    for name in [AGENT, PRODUCER] {
        let commit = trunk.test_box.registry_commit(
            &trunk.repo,
            &trunk.keys,
            &files(&trunk.keys, &key_line(name, &other)),
        );
        let why = Registry::load_at(&git, &commit).unwrap_err();
        assert!(
            why.contains(&format!("approver {name} is also enrolled in")),
            "{why}"
        );
    }
    // Control: the enrolled approver under its own name and key loads.
    let commit = trunk.test_box.registry_commit(
        &trunk.repo,
        &trunk.keys,
        &files(&trunk.keys, &key_line(APPROVER, &trunk.approver_key)),
    );
    Registry::load_at(&git, &commit).unwrap();
}

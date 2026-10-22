//! G278 (N-21): approvals-only signing needs no executor key (owner ruling 2026-10-21).
//!
//! `[policy] signing = "approvals"` at the accepted base lets a small team land with
//! `bay merge --wait` on their own machines under a key registry, with no executor, producer or
//! agent key anywhere: landings, requests, chains and check evidence are unsigned and count
//! unsigned, and results say `trust: "approvals"`. An `approval` record still counts only when
//! a key of the registry's approver file signed it: one signed by another key, or unsigned,
//! never counts, and the bay host refuses it. With `signing` absent the same machine is refused
//! as before, and a candidate that writes `signing = "approvals"` into its own policy does not
//! switch the mode it is judged by.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/publish.rs` `Ctx::executor_signer`: `if self.signing().is_none()` back to
//!   `if self.registry.is_none()` (the land refuses for want of `bay.executorKey`).
//! - `src/evidence.rs` `signed_kind`: `registry.signs_all() || kind == KIND_APPROVAL` to `true`
//!   (unsigned check evidence never counts), or to `registry.signs_all()` (the forged and the
//!   unsigned approval count).
//! - `src/hook.rs` `Judge::evidence`: `evidence::signed_kind(self.registry, &record.kind) &&`
//!   to `false &&` (the host takes a forged approval), and `pre_receive`'s
//!   `if !registry.signs_all()` to `if false` (the host refuses the unsigned chain).
//! - `src/policy.rs` `signing_at`: `policy.map_or(Signing::All, |policy| policy.signing)` to
//!   `Signing::Approvals` (the control is no longer refused for want of `bay.executorKey`).
//!   The text reader `signing_in` is held by the unit test
//!   `signing_is_all_or_approvals_and_unknown_is_all`.
//! - `src/registry.rs` `Registry::current`: drop `registry.mode = ...` (every case runs signed).
//! - `src/publish.rs` `verify_publication`: `return !crate::registry::landing_signed(..)` to
//!   `return false` (an unsigned landing is not read as landed).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::talk::fresh_key;
use support::{
    AGENT, REGISTRAR, TestBox, TestKeys, check_files, key_line, passing_result, result_json, row,
};

const APPROVER: &str = "approver-a";
const CHECK: &str = "billing-approval";
const APPROVAL: &str = "\n[[check]]\nid = \"billing-approval\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\"billing/\"]\n";
const APPROVALS_ONLY: &str = "\n[policy]\nsigning = \"approvals\"\n";
const NOTES: &str = "refs/notes/bay/evidence/approver-a";

struct Trunk {
    test_box: TestBox,
    repo: PathBuf,
    keys: TestKeys,
    /// The approver's own clone, with `bay.approverName` and `bay.approverKey`.
    approver_box: PathBuf,
}

/// A trunk on a bay host whose policy runs `tests` on every change and asks an approval for
/// `billing/`, with `[policy] signing = "approvals"` when `approvals_only`. The registry enrols
/// the agent (for the control), approver `approver-a` and the registrar; no executor and no
/// producer. The work repository sets no key at all.
fn trunk(approvals_only: bool) -> Trunk {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let mut check = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut check {
        if path == ".bay/policy.toml" {
            text.push_str(APPROVAL);
            if approvals_only {
                text.push_str(APPROVALS_ONLY);
            }
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
    Trunk {
        test_box,
        repo,
        keys,
        approver_box,
    }
}

impl Trunk {
    /// Starts a change under `intent` on `path`'s directory and writes `text` to `path` in its
    /// view. Returns the change id and its view.
    fn start(&self, intent: &str, path: &str, text: &str) -> (String, PathBuf) {
        let (code, started) = self.try_start(intent, path);
        assert_eq!(code, 0, "{started}");
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(path), text).unwrap();
        (id, view)
    }

    fn try_start(&self, intent: &str, path: &str) -> (i32, serde_json::Value) {
        let dir = format!("{}/", Path::new(path).parent().unwrap().display());
        result_json(
            self.test_box
                .bay(&self.repo)
                .args(["start", intent, "--paths", &dir]),
        )
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

    fn trunk_tip(&self) -> String {
        self.test_box
            .git_ok(&self.repo, &["rev-parse", "refs/remotes/origin/main"])
    }

    fn doctor(&self) -> serde_json::Value {
        result_json(
            self.test_box
                .bay(&self.repo)
                .args(["doctor", "--role", "executor"]),
        )
        .1
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn approvals_only_lands_without_executor_keys_and_counts_only_an_approvers_signature() {
    let trunk = trunk(true);
    // No key on this machine: doctor names the mode and needs no executor or producer key.
    for key in ["bay.agentKey", "bay.executorKey", "bay.producerKey"] {
        let (code, _, _) = trunk_config(&trunk, key);
        assert_ne!(code, 0, "{key} is set");
    }
    let doctor = trunk.doctor();
    assert_eq!(row(&doctor, "signing")["mode"], "approvals", "{doctor}");
    assert_eq!(row(&doctor, "executor-keys")["state"], "green", "{doctor}");

    // A change off the gated path lands unsigned, its evidence counted unsigned.
    let (id, view) = trunk.start("CARD-src", "src/other.rs", "fn other() {}\n");
    let (code, landed) = trunk.land(&view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(landed["detail"]["trust"], "approvals", "{landed}");
    let tip = trunk.trunk_tip();
    assert_eq!(
        trunk.test_box.git_ok(
            &trunk.repo,
            &[
                "log",
                "-1",
                "--format=%(trailers:key=Registry-Id,valueonly)",
                &tip
            ]
        ),
        "none"
    );
    let status = trunk.test_box.bay_ok(&trunk.repo, &["status", &id]);
    assert_eq!(
        status["detail"]["changes"][0]["state"], "landed",
        "{status}"
    );

    // A change on the gated path waits for a person's yes, naming who can give it.
    let (id, view) = trunk.start("CARD-billing", "billing/discount.txt", "10%\n");
    let (code, waiting) = trunk.land(&view);
    assert_eq!(code, 10, "{waiting}");
    assert_eq!(waiting["reason"], "APPROVAL_PENDING", "{waiting}");
    assert_eq!(waiting["detail"]["trust"], "approvals", "{waiting}");
    let approval = &waiting["detail"]["approval"];
    assert_eq!(approval["checks"][0]["check"], CHECK, "{waiting}");
    assert_eq!(
        approval["checks"][0]["approvers"],
        serde_json::json!([APPROVER]),
        "{waiting}"
    );
    assert!(approval["why"].is_null(), "{waiting}");
    let candidate = approval["commit"].as_str().unwrap().to_owned();
    let tree = approval["tree"].as_str().unwrap().to_owned();
    let before = trunk.trunk_tip();

    // A forged approval: the approver's name, a key the approver file does not enrol. The host
    // refuses it, and held here it never counts.
    let git = |args: &[&str]| trunk.test_box.git_ok(&trunk.repo, args);
    git(&["config", "bay.approverName", APPROVER]);
    git(&[
        "config",
        "bay.approverKey",
        trunk.keys.rogue.to_str().unwrap(),
    ]);
    let (code, forged) = trunk.approve(&trunk.repo, "pass", &candidate);
    assert_ne!(code, 0, "a forged approval was pushed: {forged}");
    assert!(forged.to_string().contains("not pushed"), "{forged}");

    // An unsigned approval: the same record with its signature taken off. The host refuses it.
    let note = git(&["notes", &format!("--ref={NOTES}"), "show", &tree]);
    let mut records: Vec<serde_json::Value> = serde_json::from_str(&note).unwrap();
    assert!(!records.is_empty(), "{note}");
    for record in &mut records {
        assert!(
            record
                .as_object_mut()
                .unwrap()
                .remove("signature")
                .is_some()
        );
    }
    let unsigned = serde_json::to_string_pretty(&records).unwrap();
    git(&[
        "notes",
        &format!("--ref={NOTES}"),
        "add",
        "-f",
        "-m",
        &unsigned,
        &tree,
    ]);
    let pushed = trunk
        .test_box
        .git(&trunk.repo)
        .args(["push", "-q", "origin", NOTES])
        .output()
        .unwrap();
    assert!(
        !pushed.status.success(),
        "the host took an unsigned approval"
    );
    for key in ["bay.approverName", "bay.approverKey"] {
        git(&["config", "--unset", key]);
    }
    let (code, still) = trunk.land(&view);
    assert_eq!(code, 10, "an unsigned approval landed the change: {still}");
    assert_eq!(still["reason"], "APPROVAL_PENDING", "{still}");
    assert_eq!(trunk.trunk_tip(), before, "trunk did not move");

    // The enrolled approver approves that exact candidate from its own clone, and it lands.
    let status = trunk.test_box.bay_ok(&trunk.approver_box, &["status", &id]);
    let named = status["detail"]["changes"][0]["operation"]["approval"]["commit"]
        .as_str()
        .unwrap()
        .to_owned();
    let (code, approved) = trunk.approve(&trunk.approver_box, "pass", &named);
    assert_eq!(code, 0, "{approved}");
    let (code, landed) = trunk.land(&view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    assert_eq!(landed["detail"]["trust"], "approvals", "{landed}");
}

#[test]
fn with_signing_absent_the_same_machine_is_refused_and_a_candidate_never_switches_its_mode() {
    let trunk = trunk(false);
    let doctor = trunk.doctor();
    assert_eq!(row(&doctor, "signing")["mode"], "all", "{doctor}");
    assert_eq!(row(&doctor, "executor-keys")["state"], "red", "{doctor}");

    // As before: with no agent key the change cannot even start.
    let (code, refused) = trunk.try_start("CARD-src", "src/other.rs");
    assert_ne!(code, 0, "{refused}");
    assert!(refused.to_string().contains("bay.agentKey"), "{refused}");

    // With the agent key alone, a change that writes `signing = "approvals"` into its own policy
    // is still judged under the base's mode: refused for want of an executor key.
    trunk.test_box.git_ok(
        &trunk.repo,
        &["config", "bay.agentKey", trunk.keys.agent.to_str().unwrap()],
    );
    let policy = trunk
        .test_box
        .git_ok(
            &trunk.repo,
            &["show", "refs/remotes/origin/main:.bay/policy.toml"],
        )
        .replacen("revision = 1", "revision = 2", 1);
    let before = trunk.trunk_tip();
    let (_, view) = trunk.start(
        "CARD-policy",
        ".bay/policy.toml",
        &format!("{policy}\n{APPROVALS_ONLY}"),
    );
    let (code, refused) = trunk.land(&view);
    assert_ne!(code, 0, "{refused}");
    assert_ne!(refused["decision"], "merged", "{refused}");
    assert!(refused.to_string().contains("bay.executorKey"), "{refused}");
    assert_eq!(trunk.trunk_tip(), before, "trunk did not move");
}

/// `git config --get <key>` in the work repository: (exit code, stdout, stderr).
fn trunk_config(trunk: &Trunk, key: &str) -> (i32, String, String) {
    let out = trunk
        .test_box
        .git(&trunk.repo)
        .args(["config", "--get", key])
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

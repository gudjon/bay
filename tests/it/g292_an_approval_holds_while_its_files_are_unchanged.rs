//! G292: an approval holds while the files it covers are unchanged (owner ruling 2026-10-21).
//!
//! A person approves the change to the gated files, not the unrelated files around it. On a
//! busy trunk every unrelated landing makes a new candidate, so an approval that answered only
//! its exact candidate would ask the person again and again. An approval record holds the
//! footprint stamp of its check's `when_paths` (`crate::throughput::stamp`): their content, byte
//! for byte, in the candidate's tree and in its base's. It answers a later candidate of the
//! same change with the same stamp, as its approver signed it: no record is written, the person
//! is not asked again, and the landing names the record under `carried_approvals[].carried_from`.
//! A landing that changes a gated file sets the approval aside: the change waits
//! `APPROVAL_PENDING` again, and the refusal and `bay status` name the gated paths that changed
//! (`detail.approval.checks[].set_aside[].paths`). `[[check]] carry = false` asks an approval of
//! every candidate, as before.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/throughput.rs` `carried_approvals`: drop `Verdict::Pass if same =>
//!   passes.push(record.clone()),` (nothing carries: the unrelated landing waits again).
//! - `src/throughput.rs` `carried_approvals`: `Verdict::Pass if same =>` to `Verdict::Pass =>`
//!   (an approval whose gated files changed lands the change).
//! - `src/policy.rs` `CheckEntry::carries`: `self.carry != Some(false)` to `self.carry ==
//!   Some(true)` (off by default: the unrelated landing waits again), or to `true` (`carry =
//!   false` lands on the old approval).
//! - `src/outside.rs` `build`: drop `unknown.insert(crate::throughput::FOOTPRINT.to_owned(),
//!   stamp);` (an approval holds no stamp, so nothing carries).
//! - `src/hook.rs` `evidence_passes`: drop `records.extend(carried.records);` (the host refuses
//!   the publication, and nothing lands).
//! - `src/publish.rs` `approval_rows`: drop `row["set_aside"] = json!(set_aside);` (the refusal
//!   and `bay status` no longer name the gated paths that changed).
//! - `src/publish.rs` landing: drop `detail["carried_approvals"] = json!(carried)` (the landing
//!   no longer names the record it stands on).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
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
    /// The approver's own clone, with `bay.approverName` and `bay.approverKey`.
    approver_box: PathBuf,
}

/// The registry files: the test registry, with `approvers` as the whole approver file.
fn files(keys: &TestKeys, approvers: &str) -> Vec<(&'static str, String)> {
    vec![
        ("agent", key_line(AGENT, &keys.agent)),
        ("executor", key_line(EXECUTOR, &keys.executor)),
        ("producer", key_line(PRODUCER, &keys.producer)),
        ("approver", approvers.to_owned()),
        ("registry", key_line(REGISTRAR, &keys.registry)),
    ]
}

/// A trunk on a bay host whose policy runs `tests` on every change and asks an approval for
/// `billing/`, with `extra` keys on the approval check; the registry enrols approver
/// `approver-a`. The work repository holds the agent, executor and producer keys, and runs the
/// executor.
fn trunk(extra: &str) -> Trunk {
    let test_box = TestBox::new();
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let mut check = check_files("true", &passing_result(1), 1, 60);
    for (path, text) in &mut check {
        if path == ".bay/policy.toml" {
            text.push_str(APPROVAL);
            text.push_str(extra);
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
        &files(&keys, &key_line(APPROVER, &approver_key)),
    );
    test_box.use_keys(&repo, &keys);
    let approver_box = test_box.clone_of(&host, "approver-box");
    test_box.git_ok(&approver_box, &["config", "bay.approverName", APPROVER]);
    test_box.git_ok(
        &approver_box,
        &["config", "bay.approverKey", approver_key.to_str().unwrap()],
    );
    Trunk {
        test_box,
        repo,
        approver_box,
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

    fn land(&self, view: &Path) -> (i32, Value) {
        result_json(self.test_box.bay(view).args(["land", "--wait"]))
    }

    /// The approver reads change `id` from its own clone and says yes to candidate `commit`.
    fn approve(&self, id: &str, commit: &str) {
        self.test_box.bay_ok(&self.approver_box, &["status", id]);
        let (code, recorded) = result_json(self.test_box.bay(&self.approver_box).args([
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
        assert_eq!(code, 0, "{recorded}");
    }

    /// A change writing `path` held `APPROVAL_PENDING`: its id, its view, and the candidate the
    /// refusal names.
    fn held(&self, intent: &str, path: &str) -> (String, PathBuf, String) {
        let (id, view) = self.start(intent, path);
        let (code, waiting) = self.land(&view);
        assert_eq!(code, 10, "{waiting}");
        (id, view, pending_candidate(&waiting))
    }

    /// One executor pass from the work repository: its row for change `id`, if it ran one.
    fn pass(&self, id: &str) -> Option<Value> {
        let ran = self
            .test_box
            .bay_ok(&self.repo, &["executor", "run", "--once"]);
        ran["detail"]["ran"]
            .as_array()
            .unwrap_or_else(|| panic!("no ran rows: {ran}"))
            .iter()
            .find(|row| row["change"] == id)
            .cloned()
    }

    /// The commit the remote's trunk names.
    fn trunk_tip(&self) -> String {
        let listed = self
            .test_box
            .git_ok(&self.repo, &["ls-remote", "origin", "refs/heads/main"]);
        listed.split_whitespace().next().unwrap().to_owned()
    }
}

/// The candidate commit an `APPROVAL_PENDING` refusal names for `billing-approval`.
fn pending_candidate(result: &Value) -> String {
    assert_eq!(result["reason"], "APPROVAL_PENDING", "{result}");
    let approval = &result["detail"]["approval"];
    assert_eq!(approval["checks"][0]["check"], CHECK, "{result}");
    assert_eq!(approval["checks"][0]["state"], "pending", "{result}");
    approval["commit"].as_str().unwrap().to_owned()
}

/// The gated paths the first earlier approval set aside on `approval`'s first check names.
fn set_aside_paths(approval: &Value) -> Vec<String> {
    approval["checks"][0]["set_aside"][0]["paths"]
        .as_array()
        .unwrap_or_else(|| panic!("no set-aside approval: {approval}"))
        .iter()
        .map(|path| path.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn an_unrelated_landing_keeps_the_approval_and_the_landing_names_it() {
    let trunk = trunk("");
    let (id, _view, first) = trunk.held("CARD-billing", "billing/discount.txt");
    trunk.approve(&id, &first);

    // Trunk moves on a file the approval does not cover, before any pass.
    let (_, other) = trunk.start("CARD-src", "src/other.rs");
    let (code, landed) = trunk.land(&other);
    assert_eq!(code, 0, "{landed}");
    let moved = trunk.trunk_tip();

    // One pass rebuilds the change on the new trunk and lands it on the approval it had.
    let row = trunk
        .pass(&id)
        .expect("the pass asks the waiting change again");
    assert_eq!(row["decision"], "landed", "{row}");
    let tip = trunk.trunk_tip();
    assert_eq!(row["detail"]["commit"], tip.as_str(), "{row}");
    assert_ne!(tip, first, "a new candidate landed, not the approved one");
    let parent = trunk
        .test_box
        .git_ok(&trunk.repo, &["rev-parse", &format!("{tip}^1")]);
    assert_eq!(parent, moved, "it landed on the moved trunk");
    let carried = &row["detail"]["carried_approvals"][0];
    assert_eq!(carried["check"], CHECK, "{row}");
    assert_eq!(carried["commit"], first.as_str(), "{row}");
    assert_eq!(carried["approver"], APPROVER, "{row}");
    assert!(
        carried["carried_from"]
            .as_str()
            .is_some_and(|record| record.starts_with(&format!("{APPROVER}/"))),
        "the landing names the approval it stands on: {row}"
    );
}

#[test]
fn a_landing_that_changes_a_gated_file_asks_again_and_names_the_path() {
    let trunk = trunk("");
    let (id, _view, first) = trunk.held("CARD-billing", "billing/discount.txt");
    trunk.approve(&id, &first);

    // Another change edits a gated file the first one does not touch, and lands approved.
    let (rates, rates_view, rates_candidate) = trunk.held("CARD-rates", "billing/rates.txt");
    trunk.approve(&rates, &rates_candidate);
    let (code, landed) = trunk.land(&rates_view);
    assert_eq!(code, 0, "{landed}");
    let moved = trunk.trunk_tip();

    // The approval covered the old `billing/rates.txt`: it is set aside, and the path named.
    let row = trunk
        .pass(&id)
        .expect("the pass asks the waiting change again");
    assert_eq!(row["decision"], "blocked", "{row}");
    let second = pending_candidate(&row);
    assert_ne!(second, first, "the refusal names the new candidate: {row}");
    assert_eq!(
        trunk.trunk_tip(),
        moved,
        "nothing landed on the old approval"
    );
    let approval = &row["detail"]["approval"];
    assert_eq!(
        set_aside_paths(approval),
        vec!["billing/rates.txt".to_owned()],
        "{row}"
    );
    assert_eq!(
        approval["checks"][0]["set_aside"][0]["commit"],
        first.as_str(),
        "{row}"
    );

    // `bay status` says the same.
    let status = trunk.test_box.bay_ok(&trunk.repo, &["status", &id]);
    let approval = &status["detail"]["changes"][0]["operation"]["approval"];
    assert_eq!(approval["commit"], second.as_str(), "{status}");
    assert_eq!(
        set_aside_paths(approval),
        vec!["billing/rates.txt".to_owned()],
        "{status}"
    );
}

#[test]
fn with_carry_false_an_unrelated_landing_needs_a_new_approval() {
    let trunk = trunk("carry = false\n");
    let (id, _view, first) = trunk.held("CARD-billing", "billing/discount.txt");
    trunk.approve(&id, &first);

    let (_, other) = trunk.start("CARD-src", "src/other.rs");
    let (code, landed) = trunk.land(&other);
    assert_eq!(code, 0, "{landed}");
    let moved = trunk.trunk_tip();

    let row = trunk
        .pass(&id)
        .expect("the pass asks the waiting change again");
    assert_eq!(row["decision"], "blocked", "{row}");
    let second = pending_candidate(&row);
    assert_ne!(second, first, "the refusal names the new candidate: {row}");
    assert_eq!(
        trunk.trunk_tip(),
        moved,
        "nothing landed on the old approval"
    );
    assert!(
        row["detail"]["approval"]["checks"][0]["set_aside"].is_null(),
        "an approval per candidate carries nothing to set aside: {row}"
    );

    // The approval of the new candidate lands it.
    trunk.approve(&id, &second);
    let row = trunk.pass(&id).expect("the pass runs the approved change");
    assert_eq!(row["decision"], "landed", "{row}");
    assert_eq!(trunk.trunk_tip(), second);
    assert!(row["detail"]["carried_approvals"].is_null(), "{row}");
}

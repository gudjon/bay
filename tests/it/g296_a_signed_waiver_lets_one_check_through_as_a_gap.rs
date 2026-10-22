//! G296 (ruling N-9): a signed waiver lets one waivable check through on one candidate, named
//! as a coverage gap, never as a pass.
//!
//! A `[[check]]` marked `waivable = true` that fails blocks the change `CHECK_FAILED`, and
//! `bay waiting` lists it with the command that waives it. `bay approve <change> --waive
//! <check> --why <text>` records the enrolled approver's signed `waiver` of that one check on
//! the exact pending candidate. The author's retry then lands that same candidate, and the
//! landing names the waiver under `coverage_gaps` (`waived_by`, `why`, `record`) with the
//! check's real verdict, `product_failure`; no record and no `say` calls it a pass. A check
//! without `waivable = true` takes no waiver. A waiver signed by a key the registry's approver
//! file does not enrol never counts. A waiver names one candidate and never carries: one about
//! an earlier candidate does not answer a new one, even on the same tree. A target never
//! counts a waiver: the waived trunk commit is not promotable to a target requiring the check.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/publish.rs` `persist_attempt`: drop the N-9 push of a refused check's evidence
//!   (`bay waiting` on the approver's box reads `unknown`: the first case).
//! - `src/policy.rs` `Policy::gaps`: drop the line
//!   `.filter(|(id, _)| self.waiver(records, id, commit).is_none())` (the waived change stays
//!   `CHECK_FAILED`: the first case).
//! - `src/policy.rs` `Policy::waiver`: drop `&& record.subject.commit == commit` (the waiver of
//!   the earlier candidate lands the new one: the fourth case).
//! - `src/evidence.rs` `signed_kind`: `registry.signs_all() || approver_kind(kind)` to
//!   `registry.signs_all() || kind == KIND_APPROVAL` (under `signing = "approvals"` the
//!   agent-signed waiver is taken unsigned, pushed and counted: the third case).
//! - `src/approve.rs` `waive_in`: `if !marked {` to `if false {` (the refusal of a check that
//!   is not waivable no longer names why and the recovery: the second case).
//! - `src/publish.rs` `approved_candidate`: drop `&& !waiver_answers(ctx, op_id)?` from
//!   `if ctx.approved.as_deref() != Some(op_id) && !waiver_answers(ctx, op_id)? {` (the retry
//!   builds a new candidate the waiver does not name, and the change stays `CHECK_FAILED`: the
//!   first case).
//! - `src/publish.rs` `coverage_gaps`: drop the `waiver_gaps` loop (the landing names no
//!   waiver: the first case).
//! - `src/result.rs` `BayResult::say`: drop the `Decision::Landed if waived(detail)` arm (the
//!   landing says "checked": the first case).
//! - `src/target.rs` `check_failures`: `.filter(|row| row["state"] != "pass")` to also skip
//!   `product_failure` (the waived commit becomes promotable: the first case).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::AGENT;
use support::UNSANDBOXED_CHOICE;
use support::approval::{APPROVER, Trunk, trunk, trunk_with, trunk_with_config};

/// The `tests` check's own key that makes it waivable.
const WAIVABLE: &str = "waivable = true\n";
/// The `staging` target, requiring the waivable check.
const STAGING: &str =
    "trunk = \"main\"\n\n[[target]]\nname = \"staging\"\nrequires = [\"tests\"]\n";

/// `trunk` on a box whose global config names no producer, as a person's machine has none: the
/// author's and the approver's clones each take their own name (G297), so the approver reads
/// the author's evidence as another writer's.
fn unnamed(trunk: Trunk) -> Trunk {
    std::fs::write(&trunk.test_box.global_config, UNSANDBOXED_CHOICE).unwrap();
    trunk
}

/// A change on `src/` that holds `src/fail`, which the `tests` check fails, landed once: it is
/// blocked `CHECK_FAILED`. Its id and view.
fn failed_change(trunk: &Trunk) -> (String, PathBuf) {
    let (id, view) = trunk.start(
        "CARD-fail",
        &["src/"],
        &[("src/fail", "1\n"), ("src/b.rs", "fn b() {}\n")],
    );
    let (code, blocked) = trunk.land(&view);
    assert_eq!(code, 10, "{blocked}");
    assert_eq!(blocked["reason"], "CHECK_FAILED", "{blocked}");
    (id, view)
}

/// The item of `bay waiting` in `dir` for change `id`.
fn waiting_item(trunk: &Trunk, dir: &Path, id: &str) -> Option<Value> {
    let listed = trunk.waiting(dir);
    listed["detail"]["waiting"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["change"] == id)
        .cloned()
}

/// The candidate commit `bay waiting` in the approver's clone names for change `id`, with its
/// `tests` row waivable and failed.
fn waivable_candidate(trunk: &Trunk, id: &str) -> String {
    let item = waiting_item(trunk, &trunk.approver_box, id)
        .unwrap_or_else(|| panic!("bay waiting lists no waivable check of {id}"));
    let row = &item["waivable"][0];
    assert_eq!(row["check"], "tests", "{item}");
    assert_eq!(row["verdict"], "product_failure", "{item}");
    assert_eq!(
        row["waive"],
        format!("bay approve {id} --waive tests --why <text>"),
        "{item}"
    );
    item["commit"].as_str().unwrap().to_owned()
}

/// `bay evidence record` of a waiver of `tests` about `commit`, from `dir`.
fn record_waiver(trunk: &Trunk, dir: &Path, commit: &str, why: &str) -> (i32, Value) {
    support::result_json(trunk.test_box.bay(dir).args([
        "evidence",
        "record",
        "--kind",
        "waiver",
        "--check",
        "tests",
        "--verdict",
        "unknown",
        "--commit",
        commit,
        "--why",
        why,
    ]))
}

/// Every record any producer holds on `tree`, read in `dir`.
fn records_on(trunk: &Trunk, dir: &Path, tree: &str) -> Vec<Value> {
    let refs = trunk.test_box.git_ok(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/notes/bay/evidence/",
        ],
    );
    let mut all = Vec::new();
    for name in refs.lines() {
        let out = trunk
            .test_box
            .git(dir)
            .args(["notes", &format!("--ref={name}"), "show", tree])
            .output()
            .unwrap();
        if out.status.success() {
            let records: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
            all.extend(records);
        }
    }
    all
}

/// Whether the remote holds a notes ref for `name`, read from `dir`.
fn pushed(trunk: &Trunk, dir: &Path, name: &str) -> bool {
    !trunk
        .test_box
        .git_ok(
            dir,
            &[
                "ls-remote",
                "origin",
                &format!("refs/notes/bay/evidence/{name}"),
            ],
        )
        .is_empty()
}

#[test]
fn a_waived_check_lands_named_as_a_gap_with_its_real_verdict() {
    let trunk = unnamed(trunk_with_config(WAIVABLE, STAGING));
    let (id, view) = failed_change(&trunk);
    let candidate = waivable_candidate(&trunk, &id);
    let before = trunk.trunk_tip();

    let (code, said) = trunk.approve(
        &trunk.approver_box,
        &id,
        &["--waive", "tests", "--why", "flaky runner"],
    );
    assert_eq!(code, 0, "{said}");
    assert_eq!(said["decision"], "ok", "{said}");
    let detail = &said["detail"];
    assert_eq!(detail["approver"], APPROVER, "{said}");
    assert_eq!(detail["waived"], "tests", "{said}");
    assert_eq!(detail["verdict"], "product_failure", "{said}");
    assert_eq!(detail["commit"], candidate.as_str(), "{said}");
    let record = detail["records"][0].as_str().unwrap().to_owned();
    assert!(record.starts_with(&format!("{APPROVER}/waiver-")), "{said}");
    assert_eq!(trunk.trunk_tip(), before, "a waiver lands nothing itself");

    // The author's retry lands that same candidate, naming the waiver and the real verdict.
    let (code, landed) = trunk.land(&view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let gaps = landed["detail"]["coverage_gaps"]
        .as_array()
        .unwrap_or_else(|| panic!("no coverage_gaps: {landed}"));
    let gap = gaps
        .iter()
        .find(|gap| gap["check"] == "tests" && gap.get("waived_by").is_some())
        .unwrap_or_else(|| panic!("no waiver gap: {landed}"));
    assert_eq!(gap["waived_by"], APPROVER, "{landed}");
    assert_eq!(gap["why"], "flaky runner", "{landed}");
    assert_eq!(gap["record"], record.as_str(), "{landed}");
    assert_eq!(gap["verdict"], "product_failure", "{landed}");
    let say = landed["say"].as_str().unwrap_or_default();
    assert!(say.contains("waived"), "{landed}");
    assert!(!say.contains("checked"), "{landed}");

    // A target never counts the waiver: the waived trunk commit is not promotable.
    let tip = trunk.trunk_tip();
    let status = trunk.test_box.bay_ok(&trunk.repo, &["status"]);
    let staging = status["detail"]["targets"]
        .as_array()
        .unwrap_or_else(|| panic!("no detail.targets: {status}"))
        .iter()
        .find(|row| row["name"] == "staging")
        .unwrap_or_else(|| panic!("no target staging: {status}"));
    assert_ne!(staging["promotable"]["commit"], tip.as_str(), "{staging}");

    // What landed is the waived candidate, and no record on its tree calls `tests` a pass.
    let tree = trunk.test_box.git_ok(
        &trunk.repo,
        &["rev-parse", &format!("{candidate}^{{tree}}")],
    );
    let records = records_on(&trunk, &trunk.repo, &tree);
    assert!(
        records
            .iter()
            .any(|record| record["kind"] == "check" && record["verdict"] == "product_failure"),
        "{records:?}"
    );
    assert!(
        !records
            .iter()
            .any(|record| record["check"]["id"] == "tests" && record["verdict"] == "pass"),
        "{records:?}"
    );
    let waiver = records
        .iter()
        .find(|held| held["id"] == record.as_str())
        .unwrap_or_else(|| panic!("no waiver record: {records:?}"));
    assert_eq!(waiver["kind"], "waiver", "{waiver}");
    assert_eq!(waiver["verdict"], "unknown", "{waiver}");
    assert_eq!(waiver["subject"]["commit"], candidate.as_str(), "{waiver}");
    assert!(!waiver["signature"].is_null(), "{waiver}");
    assert!(waiting_item(&trunk, &trunk.approver_box, &id).is_none());
}

#[test]
fn a_check_that_is_not_waivable_takes_no_waiver() {
    let trunk = unnamed(trunk());
    let (id, _) = failed_change(&trunk);
    assert!(
        waiting_item(&trunk, &trunk.approver_box, &id).is_none(),
        "a check that is not waivable is never listed to waive"
    );

    let (code, refused) = trunk.approve(
        &trunk.approver_box,
        &id,
        &["--waive", "tests", "--why", "flaky runner"],
    );
    assert_eq!(code, 20, "{refused}");
    let detail = &refused["detail"];
    assert!(
        detail["why"]
            .as_str()
            .is_some_and(|why| why.contains("tests is not waivable")),
        "{refused}"
    );
    assert!(
        detail["recovery"]
            .as_str()
            .is_some_and(|recovery| recovery.contains("waivable = true")),
        "{refused}"
    );
    assert!(!pushed(&trunk, &trunk.approver_box, APPROVER));

    // A waiver with no reason is refused too, before anything is read.
    let trunk = unnamed(trunk_with(WAIVABLE));
    let (id, view) = failed_change(&trunk);
    let (code, refused) = trunk.approve(&trunk.approver_box, &id, &["--waive", "tests"]);
    assert_eq!(code, 20, "{refused}");
    assert!(refused.to_string().contains("--why"), "{refused}");
    assert!(!pushed(&trunk, &trunk.approver_box, APPROVER));
    let (code, still) = trunk.land(&view);
    assert_eq!(code, 10, "{still}");
    assert_eq!(still["reason"], "CHECK_FAILED", "{still}");
}

#[test]
fn a_waiver_counts_only_from_an_approver_and_only_on_its_own_candidate() {
    let trunk = unnamed(trunk_with(WAIVABLE));
    let (id, view) = failed_change(&trunk);
    let first = waivable_candidate(&trunk, &id);
    let before = trunk.trunk_tip();

    // A waiver signed by the agent's own key: the host refuses it, and where it is held
    // locally it never counts. The retry builds a new candidate, still blocked.
    let keys = trunk.test_box.keys();
    trunk
        .test_box
        .git_ok(&trunk.repo, &["config", "bay.approverName", AGENT]);
    trunk.test_box.git_ok(
        &trunk.repo,
        &["config", "bay.approverKey", keys.agent.to_str().unwrap()],
    );
    let (code, forged) = record_waiver(&trunk, &trunk.repo, &first, "forged");
    assert_ne!(code, 0, "an agent-signed waiver was pushed: {forged}");
    assert!(forged.to_string().contains("not pushed"), "{forged}");
    for key in ["bay.approverName", "bay.approverKey"] {
        trunk
            .test_box
            .git_ok(&trunk.repo, &["config", "--unset", key]);
    }
    let (code, still) = trunk.land(&view);
    assert_eq!(
        code, 10,
        "an agent-signed waiver landed the change: {still}"
    );
    assert_eq!(still["reason"], "CHECK_FAILED", "{still}");
    let second = waivable_candidate(&trunk, &id);
    assert_ne!(second, first, "the retry checks a new candidate");

    // The enrolled approver waives the earlier candidate: on the same tree, that waiver still
    // does not answer the new candidate, and the next one built is blocked too.
    let (code, earlier) = record_waiver(&trunk, &trunk.approver_box, &first, "for the first");
    assert_eq!(code, 0, "{earlier}");
    let (code, still) = trunk.land(&view);
    assert_eq!(
        code, 10,
        "a waiver of an earlier candidate carried: {still}"
    );
    assert_eq!(still["reason"], "CHECK_FAILED", "{still}");
    let third = waivable_candidate(&trunk, &id);
    assert_ne!(third, first, "{still}");
    assert_ne!(third, second, "{still}");
    assert_eq!(trunk.trunk_tip(), before, "trunk did not move");

    // A waiver of the pending candidate lands that one, naming its own record.
    let (code, waived) = trunk.approve(
        &trunk.approver_box,
        &id,
        &["--waive", "tests", "--why", "flaky runner"],
    );
    assert_eq!(code, 0, "{waived}");
    assert_eq!(waived["detail"]["commit"], third.as_str(), "{waived}");
    let record = waived["detail"]["records"][0].as_str().unwrap().to_owned();
    let (code, landed) = trunk.land(&view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let named: Vec<&Value> = landed["detail"]["coverage_gaps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|gap| gap.get("waived_by").is_some())
        .collect();
    assert_eq!(named.len(), 1, "{landed}");
    assert_eq!(named[0]["record"], record.as_str(), "{landed}");
    assert_eq!(named[0]["waived_by"], APPROVER, "{landed}");
}

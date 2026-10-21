//! G281: `bay approve <change>` signs the pending candidate (owner ruling 2026-10-21).
//!
//! One command gives a person's yes or no. `bay approve <change>` writes this machine's
//! approver's signed `approval` record, `pass`, or `product_failure` with `--reject`, for every
//! approval check that waits on the change and that approver is enrolled for, about exactly the
//! candidate commit its pending operation names, through the `bay evidence record` path (the
//! approver key signs it, and it is pushed). `--why` is the record's `why`. The result says what
//! happens next. A yes lets `bay land --wait` land the change; a no blocks it `CHECK_FAILED`,
//! naming the approver and the why. It refuses, naming why and the recovery, and writes nothing,
//! with no approver key, on a change that waits on no approval, and for an approver the
//! registry does not enrol for the check: it never answers for another name.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/approve.rs` `approve_in`: `let (Some(name), Some(_)) = approver(&repo.git) else` to
//!   `let (Some(name), _) = approver(&repo.git) else` (with a name and no key the refusal is
//!   `bay evidence record`'s bare text, with no recovery).
//! - `src/approve.rs` `approve_in`: `if mine.is_empty() {` to `if false {` (the stranger's
//!   record is attempted, and the refusal no longer names the enrolment).
//! - `src/approve.rs` `write`: `commits: vec![found.commit.clone()]` to
//!   `commits: vec![found.base.clone()]` (the record is about another commit than the
//!   candidate).
//! - `src/approve.rs` `approve_in`: the verdict `if request.reject { "product_failure" } else
//!   { "pass" }` to `"pass"` (the rejected change lands).
//! - `src/outside.rs` `build`: `why: request.why.clone()` to `why: None` (the record and the
//!   rejection lose the approver's words).
//! - `src/status.rs` `approval_pending`: `let held = ... == Some(Reason::ApprovalPending);` to
//!   `let held = true;` (an approval after the rejection is refused as answered, never naming
//!   the operation's `CHECK_FAILED`).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::{Value, json};
use support::approval::{APPROVER, CHECK, STRANGER, Trunk, trunk};

/// The records `name` holds on `tree`, read in `dir`.
fn notes(trunk: &Trunk, dir: &std::path::Path, name: &str, tree: &str) -> Vec<Value> {
    let note = trunk.test_box.git_ok(
        dir,
        &[
            "notes",
            &format!("--ref=refs/notes/bay/evidence/{name}"),
            "show",
            tree,
        ],
    );
    serde_json::from_str(&note).unwrap()
}

/// Whether the remote holds a notes ref for `name`, read from `dir`.
fn pushed(trunk: &Trunk, dir: &std::path::Path, name: &str) -> bool {
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
fn approve_signs_exactly_the_pending_candidate_and_the_change_then_lands() {
    let trunk = trunk();
    let (id, view, candidate) = trunk.held_change();
    let before = trunk.trunk_tip();

    let (code, approved) = trunk.approve(&trunk.approver_box, &id, &["--why", "fine by me"]);
    assert_eq!(code, 0, "{approved}");
    assert_eq!(approved["decision"], "ok", "{approved}");
    assert_eq!(approved["change"], id.as_str(), "{approved}");
    let detail = &approved["detail"];
    assert_eq!(detail["approver"], APPROVER, "{approved}");
    assert_eq!(detail["verdict"], "pass", "{approved}");
    assert_eq!(detail["checks"], json!([CHECK]), "{approved}");
    assert_eq!(detail["commit"], candidate.as_str(), "{approved}");
    assert!(
        !detail["records"].as_array().unwrap().is_empty(),
        "{approved}"
    );
    assert!(
        detail["next"]
            .as_str()
            .is_some_and(|next| next.contains("bay land --wait")),
        "{approved}"
    );

    // The record is the approver's, signed, about exactly that candidate, and on the remote.
    let tree = trunk.test_box.git_ok(
        &trunk.approver_box,
        &["rev-parse", &format!("{candidate}^{{tree}}")],
    );
    let records = notes(&trunk, &trunk.approver_box, APPROVER, &tree);
    let record = records
        .iter()
        .find(|record| record["kind"] == "approval" && record["check"]["id"] == CHECK)
        .unwrap_or_else(|| panic!("no approval record: {records:?}"));
    assert_eq!(record["subject"]["commit"], candidate.as_str(), "{record}");
    assert_eq!(record["verdict"], "pass", "{record}");
    assert_eq!(record["producer"]["name"], APPROVER, "{record}");
    assert_eq!(record["why"], "fine by me", "{record}");
    assert!(!record["signature"].is_null(), "{record}");
    assert!(pushed(&trunk, &trunk.approver_box, APPROVER));
    assert_eq!(
        trunk.trunk_tip(),
        before,
        "an approval lands nothing itself"
    );

    // The author's retry lands it.
    let (code, landed) = trunk.land(&view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "landed", "{landed}");
}

#[test]
fn a_rejection_blocks_the_change_naming_the_approver_and_why() {
    let trunk = trunk();
    let (id, view, _) = trunk.held_change();

    let (code, rejected) = trunk.approve(
        &trunk.approver_box,
        &id,
        &["--reject", "--why", "too steep"],
    );
    assert_eq!(code, 0, "{rejected}");
    assert_eq!(
        rejected["detail"]["verdict"], "product_failure",
        "{rejected}"
    );

    let (code, refused) = trunk.land(&view);
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHECK_FAILED", "{refused}");
    let named = refused["detail"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| {
            row["check"] == CHECK && row["approver"] == APPROVER && row["why"] == "too steep"
        });
    assert!(named, "the rejection names its approver and why: {refused}");

    // The change now waits on a repair, not an approval: a yes is refused, naming why.
    let (code, late) = trunk.approve(&trunk.approver_box, &id, &[]);
    assert_eq!(code, 20, "{late}");
    let why = late["detail"]["why"].as_str().unwrap_or_default();
    assert!(why.contains("waits on no approval"), "{late}");
    assert!(why.contains("CHECK_FAILED"), "{late}");
}

#[test]
fn approve_refuses_without_a_key_with_nothing_pending_and_for_an_unenrolled_approver() {
    let trunk = trunk();
    let (id, view, _) = trunk.held_change();
    let before = trunk.trunk_tip();

    // No approver key: an approver name alone is refused, with the recovery.
    trunk
        .test_box
        .git_ok(&trunk.repo, &["config", "bay.approverName", APPROVER]);
    let (code, refused) = trunk.approve(&trunk.repo, &id, &[]);
    assert_eq!(code, 20, "{refused}");
    let detail = &refused["detail"];
    assert!(
        detail["why"]
            .as_str()
            .is_some_and(|why| why.contains("no approver key")),
        "{refused}"
    );
    assert!(
        detail["recovery"]
            .as_str()
            .is_some_and(|recovery| recovery.contains("bay.approverKey")),
        "{refused}"
    );
    trunk
        .test_box
        .git_ok(&trunk.repo, &["config", "--unset", "bay.approverName"]);

    // A change that landed with no approval waits on none.
    let (other, other_view) =
        trunk.start("CARD-src", &["src/"], &[("src/other.rs", "fn o() {}\n")]);
    let (code, landed) = trunk.land(&other_view);
    assert_eq!(code, 0, "{landed}");
    let (code, refused) = trunk.approve(&trunk.approver_box, &other, &[]);
    assert_eq!(code, 20, "{refused}");
    assert!(
        refused["detail"]["why"]
            .as_str()
            .is_some_and(|why| why.contains("waits on no approval")),
        "{refused}"
    );

    // An approver the registry does not enrol for the check is refused, and writes nothing.
    let (code, refused) = trunk.approve(&trunk.stranger_box, &id, &[]);
    assert_eq!(code, 20, "{refused}");
    assert!(
        refused["detail"]["why"]
            .as_str()
            .is_some_and(|why| why.contains("not an enrolled approver")),
        "{refused}"
    );
    assert_eq!(
        refused["detail"]["approvals"][0]["approvers"],
        json!([APPROVER]),
        "{refused}"
    );
    assert!(!pushed(&trunk, &trunk.stranger_box, STRANGER));
    assert!(!pushed(&trunk, &trunk.stranger_box, APPROVER));

    // None of them approved: the change still waits.
    let (code, still) = trunk.land(&view);
    assert_eq!(code, 10, "{still}");
    assert_eq!(still["reason"], "APPROVAL_PENDING", "{still}");
    let after_src = trunk.trunk_tip();
    assert_ne!(after_src, before, "the src change landed");
    let listed = trunk.waiting(&trunk.approver_box);
    assert_eq!(
        listed["detail"]["waiting"][0]["change"],
        id.as_str(),
        "{listed}"
    );
}

//! G280: `bay waiting` lists what needs my approval (owner ruling 2026-10-21).
//!
//! A person who approves never reads a change chain or a candidate commit. `bay waiting` lists
//! the changes whose last operation is held `APPROVAL_PENDING` for an approval check this
//! machine's approver (`bay.approverName`) is enrolled for: each with its change id, intent,
//! actor, approval check ids, the exact candidate commit `bay status` names, the changed paths
//! under the check's `when_paths` (and no other), and a one-line summary of the candidate's
//! diff. A change that needs no approval, one refused for another reason, and one never
//! submitted are not listed. An approver enrolled for none of them is listed none, and never
//! sees another's. With no approver set, every waiting change is listed, with who can approve
//! it. The decision is `ok` either way.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/approve.rs` `Held::pending`: drop
//!   `.filter(|row| approver.is_none_or(|name| may_approve(row, name)))`, or `waiting_in`'s
//!   `found.pending(name.as_deref())` to `found.pending(None)` (the unenrolled approver is
//!   listed the change).
//! - `src/policy.rs` `CheckEntry::woken_paths`: return `changed.to_vec()` (the item lists the
//!   change's `src/` path, which no `when_paths` covers).
//! - `src/approve.rs` `diff_summary`: `Some('A') => added += 1` to `Some('A') => changed += 1`
//!   (the two added files are counted as changed).
//! - `src/status.rs` `approval_pending`: `let held = ... == Some(Reason::ApprovalPending);` to
//!   `let held = true;` (the `CARD-failing` change, refused `CHECK_FAILED` on `billing/`, is
//!   listed too).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::json;
use support::AGENT;
use support::approval::{APPROVER, CHECK, STRANGER, trunk};

#[test]
fn waiting_lists_the_held_change_with_its_candidate_paths_and_diff_and_nothing_else() {
    let trunk = trunk();
    let (id, _view, candidate) = trunk.held_change();

    // A change elsewhere lands without an approval; one refused by a failing check, and one
    // never submitted, wait for none.
    let (_, view) = trunk.start("CARD-src", &["src/"], &[("src/other.rs", "fn o() {}\n")]);
    let (code, landed) = trunk.land(&view);
    assert_eq!(code, 0, "{landed}");
    assert_eq!(landed["decision"], "merged", "{landed}");
    let (_, view) = trunk.start(
        "CARD-failing",
        &["billing/", "src/"],
        &[("billing/x.txt", "x\n"), ("src/fail", "\n")],
    );
    let (code, failed) = trunk.land(&view);
    assert_eq!(code, 10, "{failed}");
    assert_eq!(failed["reason"], "CHECK_FAILED", "{failed}");
    trunk.start("CARD-draft", &["billing/"], &[("billing/y.txt", "y\n")]);

    // The approver sees exactly the one change, with the candidate `bay status` names.
    let listed = trunk.waiting(&trunk.approver_box);
    assert_eq!(listed["detail"]["approver"], APPROVER, "{listed}");
    let items = listed["detail"]["waiting"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{listed}");
    let item = &items[0];
    assert_eq!(item["change"], id.as_str(), "{listed}");
    assert_eq!(item["intent"], "CARD-billing", "{listed}");
    assert_eq!(item["actor"], AGENT, "{listed}");
    assert_eq!(item["checks"], json!([CHECK]), "{listed}");
    assert_eq!(item["commit"], candidate.as_str(), "{listed}");
    assert_eq!(
        item["paths"],
        json!(["billing/discount.txt"]),
        "only the paths when_paths covers: {listed}"
    );
    let diff = &item["diff"];
    assert_eq!(diff["files"], 2, "{listed}");
    assert_eq!(diff["added"], 2, "{listed}");
    assert_eq!(diff["changed"], 0, "{listed}");
    assert_eq!(diff["removed"], 0, "{listed}");
    assert_eq!(diff["lines_added"], 3, "{listed}");
    assert_eq!(diff["lines_removed"], 0, "{listed}");
    assert!(
        diff["summary"]
            .as_str()
            .is_some_and(|line| !line.is_empty() && !line.contains('\n')),
        "{listed}"
    );
    let status = trunk.test_box.bay_ok(&trunk.approver_box, &["status", &id]);
    assert_eq!(
        status["detail"]["changes"][0]["operation"]["approval"]["commit"],
        candidate.as_str(),
        "{status}"
    );

    // An approver the registry does not enrol is listed nothing.
    let listed = trunk.waiting(&trunk.stranger_box);
    assert_eq!(listed["detail"]["approver"], STRANGER, "{listed}");
    assert_eq!(listed["detail"]["waiting"], json!([]), "{listed}");

    // With no approver set, every waiting change, and who can approve it.
    let listed = trunk.waiting(&trunk.repo);
    assert!(listed["detail"]["approver"].is_null(), "{listed}");
    let items = listed["detail"]["waiting"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{listed}");
    assert_eq!(items[0]["change"], id.as_str(), "{listed}");
    assert_eq!(items[0]["approvers"], json!([APPROVER]), "{listed}");

    // Once approved, nothing waits for this approver.
    let (code, approved) = trunk.approve(&trunk.approver_box, &id, &[]);
    assert_eq!(code, 0, "{approved}");
    let listed = trunk.waiting(&trunk.approver_box);
    assert_eq!(listed["detail"]["waiting"], json!([]), "{listed}");
}

//! G368 (owner ruling 2026-10-25: a change that lifts a hold merges only with a person's signed
//! yes). Only a person lifts a hold.
//!
//! A policy that declares `[assets] hold` is valid only with an approval `[[check]]` whose
//! `when_paths` covers `.bay/policy.toml`. A change to the policy is judged by the policy at its
//! base, so a change that removes or narrows a hold waits for an enrolled approver's signed yes,
//! and merges once it is given. Without that check the policy is refused `POLICY_INVALID`.
//!
//! The candidate waits on two approval checks here (the policy's and the trunk's own), so one
//! `bay approve` writes two records: each check's record has an id of its own.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/policy.rs` the hold rule in validation: drop it (the policy that holds with no approval
//!   over itself is taken).
//! - `src/outside.rs` `build`: leave the check out of an approval's id (the second record
//!   collides with the first, and the approval fails).

use crate::support;

use support::approval::{self, Trunk};
use support::{TestBox, UNCHECKED_POLICY, result_json};

/// A person's yes on every change of `.bay/`, and a hold.
const HOLDS: &str = "\n[[check]]\nid = \"policy-approval\"\nenvironment = \"people\"\nkind = \"approval\"\nwhen_paths = [\".bay/\"]\n\n[assets]\nhold = [\"matters/\"]\n";

#[test]
fn a_change_that_lifts_a_hold_waits_for_a_persons_yes() {
    let trunk: Trunk = approval::trunk_with(HOLDS);
    let policy = std::fs::read_to_string(trunk.repo.join(".bay/policy.toml")).unwrap();
    assert!(policy.contains("hold = "), "{policy}");
    // A change to the policy raises its revision (G7).
    let lifted = policy
        .replace("hold = [\"matters/\"]", "hold = []")
        .replacen("revision = 1", "revision = 2", 1);
    let (id, view) = trunk.start(
        "lift the hold",
        &[".bay/"],
        &[(".bay/policy.toml", &lifted)],
    );

    let (code, waiting) = trunk.land(&view);
    assert_eq!(code, 10, "{waiting}");
    assert_eq!(
        waiting["reason"], "APPROVAL_PENDING",
        "a hold was lifted unasked: {waiting}"
    );

    let (code, approved) = result_json(
        trunk
            .test_box
            .bay(&trunk.approver_box)
            .args(["approve", &id]),
    );
    assert_eq!(code, 0, "{approved}");
    let (code, merged) = trunk.land(&view);
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
}

#[test]
fn a_policy_that_holds_with_no_approval_over_itself_is_refused() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[(
            ".bay/policy.toml".to_owned(),
            format!("{UNCHECKED_POLICY}\n[assets]\nhold = [\"matters/\"]\n"),
        )],
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "POLICY_INVALID", "{refused}");
    assert!(
        refused
            .to_string()
            .contains("only a person's signed yes lifts a hold"),
        "{refused}"
    );
}

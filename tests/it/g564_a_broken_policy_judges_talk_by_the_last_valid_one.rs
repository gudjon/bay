//! G564 (BACKLOG #132; a fleet finding, 2026-10-29: a policy on main that broke a rule turned
//! talk off, and the refusal said only that the policy did not say `enabled = true`). A broken
//! policy judges talk as it judges a merge, by the last valid policy on main (B2). With no valid
//! policy on main, talk is off, and the refusal and `bay status --all` name the fault.
//!
//! Guards to mutate: `src/policy.rs` `talk_at`'s `LoadError::Invalid` arm (return the default
//! table) and `TalkPolicy::refusal`'s `invalid` branch (the plain sentence); `src/fleet.rs`
//! `talk_policy` (read nothing).

#![cfg(unix)]

use crate::support;

use support::talk::{from_now, hosted_talk_seeded};
use support::{TestBox, UNCHECKED_POLICY, result_json};

/// A policy that breaks a rule: an unknown top-level key (G167).
const BROKEN_POLICY: &str = "revision = 2\nunchecked = true\nbogus = true\n";

fn delivery_args(follow_up: &str) -> [&str; 8] {
    [
        "agent-b",
        "Review the parser change",
        "--kind",
        "delivery",
        "--change",
        "abcd2345",
        "--follow-up",
        follow_up,
    ]
}

#[test]
fn a_broken_policy_after_a_valid_one_keeps_talk_as_the_valid_one_says() {
    let case = hosted_talk_seeded(|test_box, a| {
        test_box.seed_trunk(
            a,
            &[
                ("src/a.rs", "fn a() {}\n"),
                (
                    ".bay/policy.toml",
                    &format!("{UNCHECKED_POLICY}\n[talk]\nenabled = true\n"),
                ),
            ],
        );
        test_box.seed_trunk(
            a,
            &[(
                ".bay/policy.toml",
                &format!("{BROKEN_POLICY}\n[talk]\nenabled = true\n"),
            )],
        );
    });
    let follow_up = from_now(3600);
    let (code, sent) = case.talk("agent", &delivery_args(&follow_up));
    assert_eq!(code, 0, "talk is on by the last valid policy: {sent}");

    // The fleet map reads the last fetch: `bay status` fetches, `--all` reads it.
    case.status("agent");
    let (_, fleet) = result_json(case.bay_as("agent").args(["status", "--all"]));
    let talk = &fleet["detail"]["talk"];
    assert_eq!(talk["enabled"], true, "{fleet}");
    assert!(talk["policy_invalid"].is_null(), "{fleet}");
}

#[test]
fn with_no_valid_policy_on_main_talk_is_off_and_says_why() {
    // A plain remote under `approvals` (G445), as g491's: no key registry or talk seed.
    let test_box = TestBox::new();
    let remote = test_box.bare_any("remote.git");
    let a = test_box.repo_without_remote_any("a");
    test_box.add_origin(&a, &remote);
    let policy = vec![(
        ".bay/policy.toml".to_owned(),
        format!("{BROKEN_POLICY}\n[policy]\nsigning = \"approvals\"\n\n[talk]\nenabled = true\n"),
    )];
    test_box.seed_with_check(&a, &[("src/a.rs", "fn a() {}\n")], &policy);
    let follow_up = from_now(3600);
    let (code, refused) = result_json(
        test_box
            .bay(&a)
            .env("BAY_ACTOR", "agent")
            .env("BAY_SESSION", "s-agent")
            .arg("talk")
            .args(delivery_args(&follow_up)),
    );
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "TALK_REFUSED", "{refused}");
    assert_eq!(refused["detail"]["talk_off"], true, "{refused}");
    assert!(
        refused["detail"]["why"]
            .as_str()
            .is_some_and(|why| why.contains("POLICY_INVALID") && why.contains("bogus")),
        "the refusal names the fault: {refused}"
    );

    result_json(test_box.bay(&a).arg("status"));
    let (_, fleet) = result_json(test_box.bay(&a).args(["status", "--all"]));
    let talk = &fleet["detail"]["talk"];
    assert_eq!(talk["enabled"], false, "{fleet}");
    assert!(
        talk["policy_invalid"]
            .as_str()
            .is_some_and(|why| why.contains("bogus")),
        "{fleet}"
    );
}

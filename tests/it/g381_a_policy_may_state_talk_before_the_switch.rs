//! G381 (owner rulings 2026-10-25: talk is off unless the policy on main turns it on, and "keep
//! it ON for our setups"; v12-landingbay found that an older bay refuses the key). A policy may
//! state `[talk]` before the switch.
//!
//! This release reads `[talk]` (`enabled`, `kinds`, `senders`, `recipients`, `max_round`,
//! `wake`) and changes nothing: talk works as before. So a repository can merge `enabled = true`
//! ahead of the release that turns talk off where it is missing. `bay status --all` names it as
//! `talk: {enabled, stated, kinds}`, so an outside reader takes bay's judgment, not the policy's
//! text. A kind bay does not know is `POLICY_INVALID`, as an unknown key is.
//!
//! Guard to mutate: `src/policy.rs` `PolicyFile`: drop the `talk` field (the policy that states
//! it is refused as `POLICY_INVALID`).

use crate::support;

use support::{TestBox, UNCHECKED_POLICY, result_json};

fn with_policy(talk: &str) -> (TestBox, std::path::PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &[(
            ".bay/policy.toml".to_owned(),
            format!("{UNCHECKED_POLICY}{talk}"),
        )],
    );
    (test_box, repo)
}

#[test]
fn a_stated_talk_table_merges_and_status_names_it() {
    let (test_box, repo) = with_policy(
        "\n[talk]\nenabled = true\nkinds = [\"discovery\", \"delivery\"]\nmax_round = 64\n",
    );
    let (_, view) = test_box.start_and_edit(&repo);
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    test_box.bay_ok(&repo, &["status"]);
    let fleet = test_box.bay_ok(&repo, &["status", "--all"]);
    let talk = &fleet["detail"]["talk"];
    assert_eq!(talk["enabled"], true, "{fleet}");
    assert_eq!(talk["stated"], true, "{fleet}");
    assert_eq!(
        talk["kinds"],
        serde_json::json!(["discovery", "delivery"]),
        "{fleet}"
    );
}

#[test]
fn an_unstated_talk_is_on_as_before_and_names_every_kind() {
    let (test_box, repo) = with_policy("");
    test_box.bay_ok(&repo, &["status"]);
    let fleet = test_box.bay_ok(&repo, &["status", "--all"]);
    let talk = &fleet["detail"]["talk"];
    assert_eq!(talk["enabled"], true, "{fleet}");
    assert_eq!(talk["stated"], false, "{fleet}");
    assert_eq!(talk["kinds"].as_array().map(Vec::len), Some(3), "{fleet}");
}

#[test]
fn an_unknown_kind_or_key_is_policy_invalid() {
    for talk in [
        "\n[talk]\nkinds = [\"chat\"]\n",
        "\n[talk]\nenabled = true\nloud = true\n",
    ] {
        let (test_box, repo) = with_policy(talk);
        let (_, view) = test_box.start_and_edit(&repo);
        let (code, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 10, "{talk}: {refused}");
        assert_eq!(refused["reason"], "POLICY_INVALID", "{talk}: {refused}");
    }
}

//! G42. The Git floor for this box's role is red below that floor.
//!
//! `ROADMAP` § 1; `DESIGN` § 15.2.

mod support;

use support::{TestBox, result_json, row};

#[test]
fn an_agent_view_below_2_40_is_red() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.fake_git_version("2.39.0");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["decision"], "blocked");
    assert_eq!(result["reason"], "GIT_BELOW_FLOOR");
    let git = row(&result, "git");
    assert_eq!(git["role"], "agent-view");
    assert_eq!(git["floor"], "2.40");
    assert_eq!(git["state"], "red");
    assert_eq!(git["recovery"], "upgrade git");

    let line = test_box.bay(&repo).arg("doctor").output().unwrap();
    // The human status line is on stderr; stdout is the result document (INTERFACES § 8).
    let line = String::from_utf8(line.stderr).unwrap();
    assert!(
        line.contains("git 2.39.0 (floor 2.40, upgrade git)"),
        "{line}"
    );
    assert!(line.contains("doctor not ready"), "{line}");
}

#[test]
fn a_bay_hosted_trunk_below_2_54_is_red() {
    let test_box = TestBox::new();
    let host = test_box.bare("host.git", "sha256", "reftable");
    test_box.fake_git_version("2.53.0");
    let (code, result) = result_json(test_box.bay(&host).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "GIT_BELOW_FLOOR");
    let git = row(&result, "git");
    assert_eq!(git["role"], "bay-hosted-trunk");
    assert_eq!(git["floor"], "2.54");
    assert_eq!(git["recovery"], "upgrade git");
}

#[test]
fn git_2_40_on_an_agent_view_passes_the_row() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.fake_git_version("2.40.0");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "git")["state"], "green");
}

#[test]
fn below_2_48_refuses_the_shared_cache_without_changing_the_floor() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.fake_git_version("2.47.1");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "git")["state"], "green");
    let cache = row(&result, "shared-cache");
    assert_eq!(cache["state"], "refused");
    assert_eq!(cache["fallback"], "full clone per cache");
}

#[test]
fn below_2_50_degrades_features_without_changing_the_floor() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.fake_git_version("2.49.0");
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "git")["state"], "green");
    assert_eq!(row(&result, "features")["state"], "degraded");
}

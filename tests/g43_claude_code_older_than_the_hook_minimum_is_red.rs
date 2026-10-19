//! G43. Claude Code older than the hook minimum is red.
//!
//! `ROADMAP` § 1; `DESIGN` § 15.2.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use support::{TestBox, result_json, row};

const CLAUDE_ADAPTER: &str = "[agent]\nproduct = \"claude\"\n";

#[test]
fn claude_older_than_2_1_302_is_red() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.200"));
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "CLAUDE_BELOW_FLOOR");
    let claude = row(&result, "claude");
    assert_eq!(claude["state"], "red");
    assert_eq!(claude["floor"], "2.1.302");
    assert_eq!(
        claude["recovery"],
        "upgrade Claude Code or run without the Claude adapter"
    );

    let line = test_box.bay(&repo).arg("doctor").output().unwrap();
    // The human status line is on stderr; stdout is the result document (INTERFACES § 8).
    let line = String::from_utf8(line.stderr).unwrap();
    assert!(
        line.contains(
            "claude 2.1.200 (floor 2.1.302, upgrade Claude Code or run without the Claude adapter)"
        ),
        "{line}"
    );
}

#[test]
fn a_configured_adapter_with_claude_absent_is_red() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(None);
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 10, "{result}");
    assert_eq!(result["reason"], "CLAUDE_ABSENT");
    let claude = row(&result, "claude");
    assert_eq!(claude["state"], "red");
    assert_eq!(claude["note"], "claude is absent");
}

#[test]
fn claude_at_the_minimum_is_green() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.commit_config(&repo, CLAUDE_ADAPTER);
    test_box.fake_claude(Some("2.1.302"));
    let (_, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(row(&result, "claude")["state"], "green");
}

//! G44. No Claude adapter and no Claude Code is green on that row.
//!
//! `ROADMAP` § 1; `DESIGN` § 15.2.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

mod support;

use support::{TestBox, result_json, row};

#[test]
fn no_adapter_and_no_claude_is_green() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.fake_claude(None);
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    let claude = row(&result, "claude");
    assert_eq!(claude["state"], "green");
    assert_eq!(claude["note"], "Claude adapter not configured");

    let line = test_box.bay(&repo).arg("doctor").output().unwrap();
    // The human status line is on stderr; stdout is the result document (INTERFACES § 8).
    let line = String::from_utf8(line.stderr).unwrap();
    assert!(line.contains("claude adapter not configured"), "{line}");
}

#[test]
fn another_products_adapter_does_not_fail_for_a_missing_claude() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    TestBox::bay_config(&repo, "[agent]\nproduct = \"another-agent\"\n");
    test_box.fake_claude(None);
    let (code, result) = result_json(test_box.bay(&repo).arg("doctor"));
    assert_eq!(code, 0, "{result}");
    assert_eq!(row(&result, "claude")["state"], "green");
}

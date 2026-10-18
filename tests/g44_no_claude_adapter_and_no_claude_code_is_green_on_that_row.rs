//! G44. No Claude adapter and no Claude Code is green on that row.
//!
//! `ROADMAP` § 1; `DESIGN` § 15.2.

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
    assert_eq!(claude["note"], "not the agent on this box");

    let line = test_box.bay(&repo).arg("doctor").output().unwrap();
    // The human status line is on stderr; stdout is the result document (INTERFACES § 8).
    let line = String::from_utf8(line.stderr).unwrap();
    assert!(line.contains("claude not the agent on this box"), "{line}");
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

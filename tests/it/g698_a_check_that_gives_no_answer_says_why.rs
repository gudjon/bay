//! G698 (a fleet finding, 2026-11-12: a change was refused `CHECK_UNKNOWN` and its agent guessed
//! at a timeout under load; the record held the real cause, "the check did not start", but
//! neither the sentence nor `bay status` showed it). A check that gives no clear answer names
//! why in the result's `say`, and `bay status` shows it on the change's receipt.
//!
//! Guard to mutate: in `src/result.rs` `Reason::say`, the `CheckUnknown` arm's use of
//! `unknown_why` (the sentence says only that a check gave no clear answer).

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

#[test]
fn a_check_that_gives_no_answer_says_why() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    // GIVEN a check whose program cannot start: its interpreter is missing
    let files: Vec<(String, String)> = check_files("true", &passing_result(1), 1, 60)
        .into_iter()
        .map(|(path, text)| {
            if path == "scripts/bay-check" {
                (path, text.replacen("#!/bin/sh", "#!/nonexistent/sh", 1))
            } else {
                (path, text)
            }
        })
        .collect();
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let (id, view) = test_box.start_and_edit(&repo);

    // WHEN its change is merged
    let (_, refused) = result_json(test_box.bay(&view).args(["merge", "--wait"]));

    // THEN the sentence says why the check gave no answer
    assert_eq!(refused["reason"], "CHECK_UNKNOWN", "{refused}");
    let say = refused["say"].as_str().unwrap();
    assert!(say.contains("(tests: "), "{say}");

    // AND so does the change's receipt in `bay status`
    let (_, status) = result_json(test_box.bay(&view).args(["status", &id]));
    let receipts = status["detail"]["changes"][0]["receipts"]
        .as_array()
        .unwrap();
    assert!(
        receipts.iter().any(|row| row["reason"] == "CHECK_UNKNOWN"
            && row["why"]
                .as_str()
                .is_some_and(|why| why.starts_with("tests: "))),
        "{status}"
    );
}

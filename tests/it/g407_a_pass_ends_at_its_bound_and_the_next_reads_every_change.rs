//! G407 (a fleet finding, 2026-10-27): runner passes ran one and a half to two hours, and a
//! change submitted after a pass began was seen by no runner until it ended (35 changes sat
//! unplaced), while post checks, which run after a pass's operations, waited as long. Past its
//! bound a pass starts no new operation; the next pass reads every chain again.
//!
//! Guard to mutate, in `src/executor.rs`: the bound check in `pass` (remove it).

#![cfg(unix)]

use crate::support;

use support::{TestBox, check_files, passing_result, result_json};

/// The operations a pass's `detail.ran` names.
fn ops(ran: &serde_json::Value) -> usize {
    ran["detail"]["ran"].as_array().map_or(0, |rows| {
        rows.iter().filter(|row| row["op_id"].is_string()).count()
    })
}

#[test]
fn a_pass_past_its_bound_leaves_the_rest_to_the_next() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let files = check_files("true", &passing_result(1), 1, 60);
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    for _ in 0..2 {
        let (_, view) = test_box.start_and_edit(&repo);
        let (code, queued) = result_json(test_box.bay(&view).arg("merge"));
        assert_eq!(code, 0, "{queued}");
    }
    let executor = test_box.clone_of(&remote, "executor");
    let pass = |test_box: &TestBox| {
        let (code, ran) = result_json(
            test_box
                .bay(&executor)
                .env("BAY_TEST_PASS_BOUND_S", "0")
                .args(["runner", "run", "--once"]),
        );
        assert_eq!(code, 0, "{ran}");
        ran
    };
    let first = pass(&test_box);
    assert_eq!(
        ops(&first),
        1,
        "a pass past its bound starts no other: {first}"
    );
    let second = pass(&test_box);
    assert_eq!(ops(&second), 1, "the next pass runs the rest: {second}");
}

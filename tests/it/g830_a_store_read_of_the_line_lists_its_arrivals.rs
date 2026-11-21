//! G830 (a fleet finding, 2026-11-20: a store read of the line said "Nothing waits" while six
//! changes had waited for a runner up to 64 min). A store read of the line took no arrivals, as
//! the arrivals came only from the remote's list of merge requests, and a sweep kept neither that
//! list nor the chains of other actors' changes. A sweep keeps both now, and a store read takes
//! the arrivals from them; it still calls nothing remote.
//!
//! Guards to mutate, each alone: in `src/line.rs` `read`, answer no arrivals for a store read;
//! in `src/status.rs` `settle_swept`, keep no merge requests, or no requested chains. Each time
//! the change that waits for a runner is not listed. The other way, taking a requested chain this
//! store already holds, is guarded by g35: the read that learns twenty changes merged lists none.

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::TestBox;

#[test]
fn a_store_read_of_the_line_lists_the_changes_no_runner_has_placed() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &support::check_files("true", &support::passing_result(1), 1, 60),
    );
    let reader = test_box.clone_of(&test_box.path().join("repo-remote.git"), "reader");

    // GIVEN a change submitted on another machine, which no runner has placed
    let started = test_box.bay_ok(&repo, &["start", "an edit", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/a.rs"), "fn a() { 1; }\n").unwrap();
    let (code, submitted) = support::result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");

    // AND this machine's status sweep, then a remote that refuses every call, each one logged
    let _ = test_box.bay_ok(&reader, &["status"]);
    support::sweeps_ended(&reader);
    let calls = test_box.path().join("remote-calls");
    test_box.wrap_git(&format!(
        "case \" $* \" in\n  *\" fetch \"*|*\" ls-remote \"*|*\" push \"*) echo \"$*\" >> '{}'; exit 1 ;;\nesac",
        calls.display()
    ));

    // WHEN the line is read at deadline zero
    let line = test_box.bay_ok(&reader, &["queue", "--deadline", "0"]);

    // THEN the change waits for a runner in it, and the read called nothing remote
    let detail = &line["detail"];
    assert_eq!(detail["from_store"], true, "{line}");
    let arriving: Vec<&str> = detail["arriving"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["change"].as_str().unwrap())
        .collect();
    assert_eq!(arriving, vec![id.as_str()], "{line}");
    assert!(
        line["say"]
            .as_str()
            .unwrap()
            .starts_with("One change waits for a runner to take it; it has waited "),
        "{line}"
    );
    assert_eq!(detail["unread"], serde_json::json!([]), "{line}");
    assert!(!calls.exists(), "the read called the remote: {line}");
}

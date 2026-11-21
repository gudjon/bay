//! G831 (a fleet finding, 2026-11-21: a panel could not tell a store read of the line that lists
//! the changes waiting for a runner from one of an older bay that lists none, and a store no
//! sweep ever ran in said "Nothing waits" with nothing read). The line's document says whether
//! it read the arrivals (`detail.arrivals_listed`), and a read that did not says so.
//!
//! Guards to mutate, each alone: in `src/line.rs` `read`, answer `arrivals_listed` true for a
//! store no sweep ran in; in `src/result.rs` `line_say`, ignore it. Each time the first case
//! fails.

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::TestBox;

/// A repository whose remote holds one change no runner has placed, and a fresh clone of that
/// remote, `reader`, that never ran bay.
fn one_arrival(test_box: &TestBox) -> PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &support::check_files("true", &support::passing_result(1), 1, 60),
    );
    let reader = test_box.clone_of(&test_box.path().join("repo-remote.git"), "reader");
    let started = test_box.bay_ok(&repo, &["start", "an edit", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/a.rs"), "fn a() { 1; }\n").unwrap();
    let (code, submitted) = support::result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    reader
}

fn store_read(test_box: &TestBox, reader: &Path) -> serde_json::Value {
    test_box.bay_ok(reader, &["queue", "--deadline", "0"])
}

#[test]
fn a_store_read_no_sweep_ran_in_says_it_did_not_read_the_arrivals() {
    // GIVEN a change that waits for a runner, and a machine that never read the remote
    let test_box = TestBox::new();
    let reader = one_arrival(&test_box);
    test_box.wrap_git("case \" $* \" in\n  *\" fetch \"*|*\" ls-remote \"*) exit 1 ;;\nesac");

    // WHEN that machine reads the line at deadline zero
    let line = store_read(&test_box, &reader);

    // THEN the read says it did not read the arrivals, and never that nothing waits
    let detail = &line["detail"];
    assert_eq!(detail["from_store"], true, "{line}");
    assert_eq!(detail["arrivals_listed"], false, "{line}");
    assert_eq!(detail["arriving"], serde_json::json!([]), "{line}");
    assert_eq!(
        line["say"],
        "No change holds a place in the line. Changes waiting for a runner are not known here: \
         this machine has not read the remote yet.",
        "{line}"
    );
}

#[test]
fn a_store_read_after_a_sweep_and_a_live_read_say_they_read_the_arrivals() {
    // GIVEN a change that waits for a runner, and a machine whose status swept the remote
    let test_box = TestBox::new();
    let reader = one_arrival(&test_box);
    let _ = test_box.bay_ok(&reader, &["status"]);
    support::sweeps_ended(&reader);

    // WHEN it reads the line from its store, then from the remote
    let stored = store_read(&test_box, &reader);
    let live = test_box.bay_ok(&reader, &["queue"]);

    // THEN both say they read the arrivals, and both list the change
    for line in [&stored, &live] {
        assert_eq!(line["detail"]["arrivals_listed"], true, "{line}");
        assert_eq!(
            line["detail"]["arriving"].as_array().unwrap().len(),
            1,
            "{line}"
        );
    }
    assert_eq!(stored["detail"]["from_store"], true, "{stored}");
    assert_eq!(live["detail"]["from_store"], false, "{live}");
}

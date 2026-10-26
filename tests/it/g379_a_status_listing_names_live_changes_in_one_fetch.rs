//! G379 (owner, 2026-10-25: "the case with 'bay status' responding to many MB of JSON is not
//! agentic friendly"; the patch part, ruled "patch first, then 0.6.0"). A status listing names
//! the live changes, in one fetch.
//!
//! A plain `bay status` outside a change's view lists the changes that are still live; a merged
//! one is left out, and `bay status <change>` still reads it whole. The listing refreshes the
//! live changes' chains in one fetch, never one fetch or two per change.
//!
//! Guards to mutate: `src/status.rs` `run`: drop the `!ended(record.state)` filter (the merged
//! changes are listed); call `refresh_chains` for a listing too (a fetch per change).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_listing_names_only_live_changes_and_fetches_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let mut merged = Vec::new();
    for n in 0..4 {
        let started = test_box.bay_ok(&repo, &["start", &format!("CARD-{n}"), "--paths", "src/"]);
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        std::fs::write(view.join(format!("src/f{n}.rs")), "fn f() {}\n").unwrap();
        let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{landed}");
        merged.push(started["change"].as_str().unwrap().to_owned());
    }
    let mut live = Vec::new();
    for n in 0..3 {
        let started = test_box.bay_ok(
            &repo,
            &[
                "start",
                &format!("LIVE-{n}"),
                "--paths",
                &format!("live{n}/"),
            ],
        );
        live.push(started["change"].as_str().unwrap().to_owned());
    }

    let fetches = |trace: &str| {
        let trace = test_box.path().join(trace);
        let (code, listed) = result_json(
            test_box
                .bay(&repo)
                .arg("status")
                .env("BAY_TEST_GIT_TRACE", &trace),
        );
        assert_eq!(code, 0, "{listed}");
        let count = std::fs::read_to_string(&trace)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("\"sub\":\"fetch\""))
            .count();
        (listed, count)
    };
    let (listed, before) = fetches("first.trace");
    let mut ids: Vec<String> = listed["detail"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["change"].as_str().unwrap_or_default().to_owned())
        .collect();
    ids.sort();
    live.sort();
    assert_eq!(ids, live, "only the live changes: {listed}");

    // Three more live changes cost the listing no more fetches.
    for n in 3..6 {
        test_box.bay_ok(
            &repo,
            &[
                "start",
                &format!("LIVE-{n}"),
                "--paths",
                &format!("live{n}/"),
            ],
        );
    }
    let (_, after) = fetches("second.trace");
    assert_eq!(
        after, before,
        "a fetch per change: {before} fetches, then {after}"
    );

    // A merged change is still read whole when it is named.
    let named = test_box.bay_ok(&repo, &["status", &merged[0]]);
    assert_eq!(named["detail"]["changes"][0]["state"], "merged", "{named}");
}

/// A fleet finding (2026-10-26: a listing of 146 live changes ran to megabytes, and an agent reads
/// it whole): a listing describes at most 20 changes, the reader's own first, and names the rest
/// with how to read one.
#[test]
fn a_listing_describes_at_most_twenty_changes_and_names_the_rest() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    for n in 0..23 {
        test_box.bay_ok(
            &repo,
            &["start", &format!("CARD-{n}"), "--paths", &format!("p{n}/")],
        );
    }
    let (code, listed) = result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 0, "{listed}");
    let described = listed["detail"]["changes"].as_array().unwrap().len();
    assert_eq!(described, 20, "{listed}");
    assert_eq!(listed["detail"]["more_changes"]["count"], 3, "{listed}");
    assert_eq!(
        listed["detail"]["more_changes"]["read"], "bay status <change>",
        "{listed}"
    );
}

/// A fleet finding (2026-10-26: one status on a shared box took five minutes and 80 KB, for an
/// agent that wanted a few hundred tokens): a listing describes the reader's own changes, and
/// counts and names everyone else's.
#[test]
fn a_listing_describes_only_the_readers_own_changes() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    for n in 0..3 {
        let (code, started) =
            result_json(test_box.bay(&repo).env("BAY_ACTOR", "someone-else").args([
                "start",
                &format!("THEIRS-{n}"),
                "--paths",
                &format!("t{n}/"),
            ]));
        assert_eq!(code, 0, "{started}");
    }
    for n in 0..2 {
        test_box.bay_ok(
            &repo,
            &["start", &format!("MINE-{n}"), "--paths", &format!("m{n}/")],
        );
    }
    let (code, listed) = result_json(test_box.bay(&repo).arg("status"));
    assert_eq!(code, 0, "{listed}");
    let described = listed["detail"]["changes"].as_array().unwrap();
    assert_eq!(described.len(), 2, "only the reader's own: {listed}");
    assert_eq!(listed["detail"]["more_changes"]["count"], 3, "{listed}");
}

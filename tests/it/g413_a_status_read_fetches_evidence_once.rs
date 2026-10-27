//! G413 (a fleet finding, 2026-10-27: one `bay status <change>` on a hosted remote made ten round
//! trips, four of them for evidence notes: a listing of the remote's notes refs, then a fetch of
//! them, and the pair again later in the same read). A status read fetches the evidence notes in
//! one round trip.
//!
//! Guards to mutate: `src/repo.rs` `fetch_evidence` listing the refs before it fetches;
//! `src/status.rs` calling `fetch_evidence` rather than `fetch_evidence_once`.

#![cfg(unix)]

use crate::support;

use std::path::Path;

use support::queued::{Check, OTHER_PRODUCER, host};
use support::result_json;

const CONFIG: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\n";

/// The git calls of a trace2 event log that talk to a remote about the evidence notes.
fn evidence_round_trips(trace: &Path) -> Vec<String> {
    std::fs::read_to_string(trace)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(r#""event":"start""#))
        .filter(|line| line.contains(r#""fetch""#) || line.contains(r#""ls-remote""#))
        .filter(|line| line.contains("refs/notes/bay/evidence"))
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_status_read_fetches_the_evidence_notes_in_one_round_trip() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let test_box = &held.test_box;
    let id = held.queue("one", "src");
    // Another producer's runner lands it: the remote holds evidence this clone did not write.
    let runner = held.executor("runner", OTHER_PRODUCER);
    let merged = held.pass(&runner);
    assert_eq!(
        test_box.trunk_change_ids(&held.remote),
        vec![id.clone()],
        "{merged}"
    );
    let trace = test_box.path().join("trace.json");
    let (code, status) = result_json(
        test_box
            .bay(&held.repo)
            .env("GIT_TRACE2_EVENT", &trace)
            .args(["status", &id]),
    );
    assert_eq!(code, 0, "{status}");
    let trips = evidence_round_trips(&trace);
    assert!(
        trips.len() <= 1,
        "{} evidence round trips: {trips:#?}",
        trips.len()
    );
}

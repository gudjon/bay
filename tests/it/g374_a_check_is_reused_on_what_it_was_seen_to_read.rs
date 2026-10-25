//! G374 (items 1 and 2, owner ruling 2026-10-25: "get this fully built"). A check is reused on
//! what it was seen to read.
//!
//! A check with `footprint = "observed"` declares no read set: bay arms its fresh tree and
//! records what the run read and listed (`crate::check::observe`) as the record's footprint.
//! A later candidate of the same change reuses the record when those files and listings are the
//! same, byte for byte, in its tree and its base's; anything the check read or listed that
//! changed, it runs again. G257's queue: the head changes `ui/`, the second `api/`; the check
//! reads `api/a.rs` and lists `lib/`.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/throughput.rs` `same_observed`: answer `true` at once (a changed listing is carried).
//! - `src/check.rs` `run_once`: never arm (no observed footprint: nothing is carried).

#![cfg(unix)]

use crate::support;

use serde_json::Value;

use support::PRODUCER;
use support::queued::{Check, Held, OTHER_PRODUCER, finished, host, ran_for};

const CONFIG: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\nfootprint_lanes = true\n";

/// The policy: check `ui` on `ui/` (declared), and check `api` on `api/`, observed.
fn policy() -> String {
    let check = |id: &str, extra: &str| {
        format!(
            "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"{id}/\"]\n{extra}"
        )
    };
    format!(
        "revision = 1\n{}{}",
        check("ui", "footprint = true\n"),
        check("api", "footprint = \"observed\"\n")
    )
}

/// G257's queue, with every check run reading `api/a.rs` and listing `lib/`. The head changes
/// `ui/` (and adds `lib/one.rs` when `head_touches_lib`). Returns the host.
fn queue(head_touches_lib: bool) -> Held {
    let policy = policy();
    let held = host(&Check {
        config: CONFIG,
        policy: Some(&policy),
        seed: &[
            ("ui/a.rs", "fn a() {}\n"),
            ("api/a.rs", "fn a() {}\n"),
            ("lib/a.rs", "fn a() {}\n"),
        ],
        hold_first: true,
        fails: "cat api/a.rs >/dev/null; ls lib >/dev/null; false",
    });
    let mut files = vec![("ui/one.rs", "fn one() {}\n")];
    if head_touches_lib {
        files.push(("lib/one.rs", "fn one() {}\n"));
    }
    let one = held.queue_with("one", &["ui", "lib"], &files);
    let two = held.queue("two", "api");
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let child = held.spawn(&a);
    held.wait_held();
    let ahead = held.pass(&b);
    assert_eq!(
        ran_for(&ahead, &two)["detail"]["precheck"]["on"],
        "tip",
        "{ahead}"
    );
    held.go();
    let outcome = finished(child);
    for id in [&one, &two] {
        assert_eq!(ran_for(&outcome, id)["decision"], "merged", "{outcome}");
    }
    held
}

#[test]
fn a_record_whose_observed_reads_are_unchanged_is_carried() {
    let held = queue(false);
    let requests = held.requests();
    assert_eq!(requests.len(), 2, "carried, not rechecked: {requests:?}");
    let landed = held.tree("main");
    let records = held.records(PRODUCER, &landed);
    let carried: &Value = records
        .iter()
        .find(|record| record["check"]["id"] == "api")
        .unwrap_or_else(|| panic!("{records:?}"));
    let footprint = &carried["footprint"];
    assert_eq!(footprint["observed"], true, "{carried}");
    let read: Vec<&str> = footprint["read"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row[0].as_str())
        .collect();
    assert!(
        read.contains(&"api/a.rs"),
        "the read file is not named: {carried}"
    );
    let listed: Vec<&str> = footprint["listed"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|row| row[0].as_str())
        .collect();
    assert!(
        listed.contains(&"lib"),
        "the listed directory is not named: {carried}"
    );
    assert_eq!(
        footprint["from"]["tree"], requests[1]["candidate"]["tree"],
        "{carried}"
    );
}

#[test]
fn a_record_whose_observed_listing_changed_is_checked_again() {
    let held = queue(true);
    assert_eq!(held.requests().len(), 3, "rechecked: {:?}", held.requests());
}

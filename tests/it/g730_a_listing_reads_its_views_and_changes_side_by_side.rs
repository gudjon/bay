//! G730 (a measurement, 2026-11-15: on a clone of 6,200 changes the longest section of a listing
//! was its leftovers, 2-3.6 s, each checked view's work tree read in turn; the next was the shown
//! changes, 0.9-1.4 s, each described in turn). A listing reads the views it checks side by side,
//! and describes the changes it shows side by side, each row in its place. Read from the trace a
//! test build writes (G171): with each `git ls-files` slowed, several views' reads are in flight
//! at one time.
//!
//! Guards to mutate: in `src/status.rs`, `LEFTOVER_THREADS` = 1 (the views are read one after
//! another, and no two `ls-files` are in flight at one time).

// The slow git is a shell script, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::{TestBox, result_json, unchecked_files};

/// The most `git <sub>` spawns of bay's own process that `trace` shows in flight at one time.
fn most_in_flight(trace: &str, sub: &str) -> usize {
    let lines: Vec<Value> = trace
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let pid = lines.first().map(|line| line["pid"].clone());
    let mut edges: Vec<(u64, i8)> = lines
        .iter()
        .filter(|line| Some(&line["pid"]) == pid.as_ref() && line["sub"] == sub)
        .filter_map(|line| {
            let end = line["end"].as_u64()?;
            Some([(end - line["us"].as_u64()?, 1), (end, -1)])
        })
        .flatten()
        .collect();
    edges.sort_unstable();
    let (mut now, mut most) = (0_i64, 0_i64);
    for (_, step) in edges {
        now += i64::from(step);
        most = most.max(now);
    }
    usize::try_from(most).unwrap_or(0)
}

/// A wrapper body that waits half a second before each `git <sub>`.
fn slowed(sub: &str) -> String {
    format!(
        "skip=\nfor arg in \"$@\"; do\n  if [ -n \"$skip\" ]; then skip=; continue; fi\n  case \"$arg\" in\n    -C|-c) skip=1;;\n    -*) ;;\n    {sub}) sleep 0.5; break;;\n    *) break;;\n  esac\ndone"
    )
}

#[test]
fn a_listing_reads_the_views_it_checks_side_by_side() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    // GIVEN four views of the reader's own changes
    let ids: Vec<String> = (0..4).map(|_| test_box.start_and_edit(&repo).0).collect();

    // WHEN a listing checks them while each read of a work tree is slow
    test_box.wrap_git(&slowed("ls-files"));
    let trace = test_box.path().join("trace.jsonl");
    let (code, status) = result_json(
        test_box
            .bay(&repo)
            .arg("status")
            .env("BAY_TEST_SAVED", "off")
            .env("BAY_TEST_GIT_TRACE", &trace),
    );
    assert_eq!(code, 0, "{status}");
    support::sweeps_ended(&repo);

    // THEN every view is listed, and several were read at one time
    let leftovers = status["detail"]["leftovers"].to_string();
    for id in &ids {
        assert!(leftovers.contains(id.as_str()), "{id} not listed: {status}");
    }
    let most = most_in_flight(&std::fs::read_to_string(&trace).unwrap(), "ls-files");
    assert!(
        most >= 2,
        "at most {most} view read was in flight at one time, so the views were read in turn"
    );
}

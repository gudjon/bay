//! G411 (a fleet finding, 2026-10-27: a runner's blob-less clone of a hosted remote fetched each
//! queue entry and change record by id as it read them, one round trip of about 2 s each, so a
//! pass over 30 places spent two minutes before any check, and a batch took ten to plan). A
//! fetch of bay's records alone brings their blobs, so a runner reads the line locally.
//!
//! Guard to mutate: `src/git.rs` `as_formatted`: a fetch of bay's records alone keeps the
//! clone's blob filter.

#![cfg(unix)]

use crate::support;

use std::path::Path;

use support::queued::{Check, host};
use support::result_json;

const CONFIG: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\n";

/// The fetches by id (`git fetch … --stdin`) any git process of a trace2 event log ran.
fn fetches_by_id(trace: &Path) -> usize {
    std::fs::read_to_string(trace)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(r#""event":"start""#))
        .filter(|line| line.contains(r#""fetch""#) && line.contains(r#""--stdin""#))
        .count()
}

#[test]
fn a_blob_less_runner_reads_every_place_without_fetching_it_by_id() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let test_box = &held.test_box;
    test_box.git_bare(&held.remote, &["config", "uploadpack.allowFilter", "true"]);
    let places = ["one", "two", "three", "four", "five", "six"];
    for (at, name) in places.iter().enumerate() {
        let _ = held.queue(name, &format!("d{at}"));
    }
    let executor = test_box.path().join("exec");
    test_box.git_ok(
        test_box.path(),
        &[
            "clone",
            "-q",
            "--filter=blob:none",
            &format!("file://{}", held.remote.display()),
            executor.to_str().unwrap(),
        ],
    );
    test_box.use_keys(&executor, &held.keys);
    let trace = test_box.path().join("trace.json");
    let (_, pass) = result_json(
        test_box
            .bay(&executor)
            .env("GIT_TRACE2_EVENT", &trace)
            .args(["runner", "run", "--once"]),
    );
    let by_id = fetches_by_id(&trace);
    assert!(
        by_id < places.len(),
        "{by_id} fetches by id for {} places: {pass}",
        places.len()
    );
}

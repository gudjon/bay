//! G853 (a fleet finding, 2026-11-22: `bay queue` said "Nothing waits to be added to main." for
//! 17 minutes while a runner checked a change it had taken off an empty line). A runner that
//! takes a change no runner placed holds it under its lease, and the line lists it nowhere: it
//! holds no place, and it is not arriving, as its operation is past `submitted`. `bay queue` now
//! lists it under `taken`, with the runner that holds it, and its sentence says a runner checks
//! it. A store read lists it too, from the leases this store last fetched. Once it merges, nothing
//! is taken.
//!
//! Guard to mutate: `src/line.rs` `Line::read`: drop `crate::lease::refspec()` from the
//! line's fetch (the reader holds no copy of the lease, and the change is listed nowhere).

use crate::support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::json;
use support::PRODUCER;
use support::queued::{Check, finished, host};

#[test]
fn a_change_a_runner_checks_with_no_place_is_listed_as_taken() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 30\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let observer = held.test_box.clone_of(&held.remote, "observer");

    // GIVEN a change submitted to an empty line
    let started = held
        .test_box
        .bay_ok(&held.repo, &["start", "lone", "--paths", "src/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/lone.rs"), "fn lone() {}\n").unwrap();
    let submitted = held.test_box.bay_ok(&view, &["merge"]);
    assert_eq!(submitted["decision"], "queued", "{submitted}");

    // WHEN a runner takes it, and its check runs under the runner's lease
    let runner = held.executor("exec-a", PRODUCER);
    let pass = held.spawn(&runner);
    held.wait_held();
    let lease = format!("refs/bay/lease/{id}");
    let start = Instant::now();
    while held
        .test_box
        .git_bare(
            &held.remote,
            &["for-each-ref", "--format=%(refname)", &lease],
        )
        .trim()
        .is_empty()
    {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the runner never beat its lease of {id}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // THEN another clone's read of the line lists it as taken by that runner, holding no place
    // and not arriving, and says a runner checks it
    let line = held.test_box.bay_ok(&observer, &["queue"]);
    let detail = &line["detail"];
    assert_eq!(detail["depth"], 0, "the runner gave it no place: {line}");
    assert_eq!(detail["arriving"], json!([]), "{line}");
    let taken = detail["taken"]
        .as_array()
        .unwrap_or_else(|| panic!("no taken list: {line}"));
    assert_eq!(taken.len(), 1, "{line}");
    assert_eq!(taken[0]["change"], id.as_str(), "{line}");
    assert_eq!(taken[0]["intent"], "lone", "{line}");
    assert_eq!(taken[0]["state"], "running", "{line}");
    assert!(taken[0]["runner"].is_string(), "{line}");
    assert!(taken[0]["beat_at"].is_string(), "{line}");
    assert!(taken[0]["waited_s"].is_u64(), "{line}");
    assert_eq!(detail["oldest_wait_s"], taken[0]["waited_s"], "{line}");
    let say = line["say"].as_str().unwrap();
    assert!(
        say.starts_with("One change waits to be added to main; it has waited "),
        "{line}"
    );
    assert!(
        say.ends_with(". A runner checks it with no place in the line."),
        "{line}"
    );
    // A store read lists it from the leases this store fetched.
    let stored = held
        .test_box
        .bay_ok(&observer, &["queue", "--deadline", "0"]);
    assert_eq!(stored["detail"]["from_store"], true, "{stored}");
    assert_eq!(
        stored["detail"]["taken"][0]["change"],
        id.as_str(),
        "{stored}"
    );

    // WHEN its check ends and it merges THEN nothing is taken, and nothing waits
    held.go();
    let ran = finished(pass);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
    let line = held.test_box.bay_ok(&observer, &["queue"]);
    assert_eq!(line["detail"]["taken"], json!([]), "{line}");
    assert_eq!(line["say"], "Nothing waits to be added to main.", "{line}");
}

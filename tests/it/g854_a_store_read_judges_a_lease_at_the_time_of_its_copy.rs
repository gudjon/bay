//! G854 (a fleet finding, 2026-11-22: `bay status` said a lease looked expired at 07:23:28 from
//! a copy of the remote fetched at 07:20:02, while its holder had beaten it at 07:23:15). A status
//! read judges a lease's age at the time its copy was fetched, as the holder's signs of life it
//! reads are that copy's. A store read whose copy is now older than the lease says `held` of a
//! lease its holder beat just before the copy; a live read says `held` as the holder still beats.
//!
//! Guard to mutate: `src/status.rs` `lease`: judge the age at now, not at `copied`, and the
//! store read says "suspected expired".

#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};

use support::PRODUCER;
use support::queued::{Check, finished, host};

#[test]
fn a_store_read_judges_a_lease_at_the_time_of_its_copy() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 5\nheartbeat_s = 1\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let observer = held.test_box.clone_of(&held.remote, "observer");

    // GIVEN a change a runner holds while its check runs, the holder beating its lease
    let id = held.queue("lone", "src");
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

    // WHEN another clone's status copies the remote, and that copy grows older than the lease
    let live = held.test_box.bay_ok(&observer, &["status", &id]);
    support::sweeps_ended(&observer);
    assert_eq!(
        live["detail"]["changes"][0]["operation"]["lease"], "held",
        "{live}"
    );
    std::thread::sleep(Duration::from_secs(7));

    // THEN a read past its sweep's window, which answers from that copy as the sweep it starts
    // outlasts its deadline (G710), says the lease is held
    held.test_box.wrap_git(
        "case \"$(pwd -P) $*\" in\n  *observer*) case \" $* \" in *\" fetch \"*) sleep 4 ;; esac ;;\nesac",
    );
    let (code, stored) = support::result_json(
        held.test_box
            .bay(&observer)
            .args(["status", &id])
            .env("BAY_TEST_SWEPT_S", "0")
            .env("BAY_TEST_STATUS_DEADLINE_S", "1"),
    );
    assert_eq!(code, 0, "{stored}");
    support::sweeps_ended(&observer);
    held.test_box.wrap_git("");
    let warned = stored["detail"]["warnings"]
        .as_array()
        .is_some_and(|warnings| {
            warnings
                .iter()
                .any(|warning| warning["code"] == "stale_read")
        });
    assert!(warned, "the read waited for its sweep: {stored}");
    assert_eq!(
        stored["detail"]["changes"][0]["operation"]["lease"], "held",
        "the copy's last beat is judged at now: {stored}"
    );

    // AND a read that copies the remote again says so too, as the holder still beats
    let (code, fresh) = support::result_json(
        held.test_box
            .bay(&observer)
            .args(["status", &id])
            .env("BAY_TEST_SWEPT_S", "0"),
    );
    assert_eq!(code, 0, "{fresh}");
    support::sweeps_ended(&observer);
    assert_eq!(
        fresh["detail"]["changes"][0]["operation"]["lease"], "held",
        "{fresh}"
    );

    held.go();
    let ran = finished(pass);
    assert_eq!(ran["detail"]["ran"][0]["decision"], "merged", "{ran}");
}

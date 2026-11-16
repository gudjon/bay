//! G760 (a gate finding, 2026-11-16: under load, G754's test failed in both formats). A runner
//! joins each member to its batch by claiming it; when that claim's push lost to a write on the
//! member's chain (its author's newer revision, or this runner placing that revision in line),
//! the claim read the member as ended, since its author's request had superseded it, and cut
//! it from the batch: the batch landed without it, and the newer revision landed in its place.
//! A member its author revised is not ended for its batch's claim: the claim is made again, and
//! the member lands with the batch as checked; the newer edits are not on main yet.
//!
//! The runner also places no newer revision of a member while that member's batch lands: its
//! write raced the batch's claim and its persist on the member's chain (the gate's failure,
//! "a member left the batch before its push"). A later pass places it.
//!
//! Guards to mutate: in `src/publish.rs` `lost_claim`, count every terminal operation as ended
//! (drop `!frozen(..)`), and the member is cut from the batch; in `src/executor.rs`
//! `place_arrival`, drop the `in_running_batch` term, and the runner places the newer revision
//! while the batch lands.

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use support::queued::{Check, finished, host};
use support::{EXECUTOR, PRODUCER, result_json};

const CONFIG: &str = "[runner]\nlease_s = 60\nheartbeat_s = 1\nbatch = { min = 2, max = 8, max_failure_rate = 1.0 }\n";

#[test]
fn a_member_revised_after_it_joined_its_batch_still_lands_as_checked() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let one = held.queue("one", "src");
    let started = held
        .test_box
        .bay_ok(&held.repo, &["start", "two", "--paths", "lib/"]);
    let two = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::create_dir_all(view.join("lib")).unwrap();
    std::fs::write(view.join("lib/two.rs"), "fn two() {}\n").unwrap();
    let (code, submitted) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    let signer = bay::signing::Signer {
        key: held.keys.executor.clone(),
        name: EXECUTOR.to_owned(),
    };
    bay::queue::join(&opened, Some(&signer), &format!("{two}.1"), &two).unwrap();

    // GIVEN a runner whose batch of one and two is checked, held as it pushes two's claim
    let runner = held.executor("exec", PRODUCER);
    let release = held.test_box.path().join("release-runner");
    let pass = held
        .test_box
        .bay_process(&runner)
        .env("BAY_TEST_PAUSE_AT", "before-member-claim-push")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["--json", "runner", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = PathBuf::from(format!("{}.paused", release.display()));
    let deadline = Instant::now() + Duration::from_secs(120);
    while !paused.exists() {
        assert!(Instant::now() < deadline, "the batch never claimed two");
        std::thread::sleep(Duration::from_millis(50));
    }

    // WHEN two's author sends a newer revision now, so the claim's push loses, and the runner
    // goes on
    std::fs::write(view.join("lib/later.rs"), "fn later() {}\n").unwrap();
    let (code, revised) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{revised}");
    std::fs::write(&release, "go").unwrap();
    let pass = finished(pass);

    // THEN both checked members land with the batch, and the newer edits are not on main yet
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    let chain = held.test_box.git_bare(
        &held.remote,
        &["log", "--format=%s", &format!("refs/bay/changes/{two}")],
    );
    assert!(
        on_main.contains(&one) && on_main.contains(&two),
        "the batch did not land every checked member: {on_main:?}\n{chain}\n{pass}"
    );
    let tree = held
        .test_box
        .git_bare(&held.remote, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(!tree.contains("lib/later.rs"), "{tree}");
}

/// G760: while a runner lands a batch, it does not place a member's newer revision in line:
/// that write raced the batch's claim and persist on the member's chain. A later pass places it.
#[test]
fn a_runner_places_no_newer_revision_of_a_member_while_its_batch_lands() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let one = held.queue("one", "src");
    let started = held
        .test_box
        .bay_ok(&held.repo, &["start", "two", "--paths", "lib/"]);
    let two = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::create_dir_all(view.join("lib")).unwrap();
    std::fs::write(view.join("lib/two.rs"), "fn two() {}\n").unwrap();
    let (code, submitted) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    let signer = bay::signing::Signer {
        key: held.keys.executor.clone(),
        name: EXECUTOR.to_owned(),
    };
    bay::queue::join(&opened, Some(&signer), &format!("{two}.1"), &two).unwrap();

    // GIVEN the batch of one and two in its check
    let runner = held.executor("exec", PRODUCER);
    let pass = held.spawn(&runner);
    held.wait_held();

    // WHEN two's author sends a newer revision meanwhile
    std::fs::write(view.join("lib/later.rs"), "fn later() {}\n").unwrap();
    let (code, revised) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{revised}");
    let newer = revised["op_id"].as_str().unwrap().to_owned();
    held.go();
    let pass = finished(pass);

    // THEN the pass placed no newer revision of the member, and the batch landed both
    let rows = pass["detail"]["ran"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        rows.iter().all(|row| row["op_id"] != newer.as_str()),
        "the runner wrote the member's newer revision while its batch landed: {pass}"
    );
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        on_main.contains(&one) && on_main.contains(&two),
        "{on_main:?} {pass}"
    );
}

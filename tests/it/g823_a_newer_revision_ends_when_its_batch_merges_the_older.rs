//! G823 (a fleet finding, 2026-11-20: a batch merged a member's checked revision while its
//! author's newer revision of the same change waited with no place in line; status said it waited
//! its turn, and nothing told the author that the newer edits were not on main). When an
//! operation of a change merges, every other operation of that change still under way ends
//! `CHANGE_ALREADY_MERGED`. An event tells its author, and its next step says that `bay merge` in
//! the change's view sends the edits made after the merged revision as the next change.
//!
//! Guard to mutate, in `src/publish.rs`: `end_behind` (end nothing: the newer operation stays
//! admitted with no place, and status says it waits its turn).

#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::queued::{Check, finished, host};
use support::{EXECUTOR, PRODUCER, result_json};

const CONFIG: &str = "[runner]\nlease_s = 60\nheartbeat_s = 1\nbatch = { min = 2, max = 8, max_failure_rate = 1.0 }\n";

#[test]
fn a_newer_revision_ends_when_its_batch_merges_the_older_and_says_merge_it_next() {
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

    // GIVEN the batch of one and two in its check, and two's author revises it meanwhile
    let runner = held.executor("exec", PRODUCER);
    let pass = held.spawn(&runner);
    held.wait_held();
    std::fs::write(view.join("lib/later.rs"), "fn later() {}\n").unwrap();
    let (code, revised) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{revised}");

    // WHEN the batch merges two's checked revision
    held.go();
    let pass = finished(pass);
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        on_main.contains(&one) && on_main.contains(&two),
        "the batch did not merge every checked member: {on_main:?} {pass}"
    );

    // THEN two's newer operation has ended, and its next step is a merge in the view
    let status = held.test_box.bay_ok(&held.repo, &["status", &two]);
    let operation = &status["detail"]["changes"][0]["operation"];
    assert_eq!(operation["op_id"], format!("{two}.2"), "{status}");
    assert_eq!(operation["state"], "blocked", "{status}");
    assert_eq!(operation["outcome"], "CHANGE_ALREADY_MERGED", "{status}");
    let next = operation["next_step"].as_str().unwrap_or_default();
    assert!(
        next.contains("`bay merge`") && !next.contains("waits its turn"),
        "{status}"
    );

    // AND an event tells the author
    let watched = held.test_box.bay_ok(&view, &["status", "--since"]);
    let told = watched["detail"]["events"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|event| {
            event["op_id"] == format!("{two}.2").as_str()
                && event["reason"] == "CHANGE_ALREADY_MERGED"
        });
    assert!(told, "no event names {two}.2: {watched}");

    // AND the author's merge in the view sends the newer edits as the next change, which merges
    let (code, again) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{again}");
    let next = again["detail"]["follow_up"]["change"]
        .as_str()
        .unwrap_or_else(|| panic!("no follow-up change: {again}"))
        .to_owned();
    let landed = held.pass(&runner);
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&next),
        "the follow-up did not merge: {landed}"
    );
    let tree = held
        .test_box
        .git_bare(&held.remote, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(tree.contains("lib/later.rs"), "{tree}");
}

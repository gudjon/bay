//! G583. A batch cut at a member that cannot join lands the members before it, as one batch.
//!
//! A runner checks a batch, then each member behind the head joins it: it verifies the member's
//! request and claims its operation. Until 0.6.9 a member that could not join (its owner sent a
//! new revision while the batch was checked, say) ended the batch: every claim went back and the
//! head ran alone, so the batch check and each join before it were lost, and the journal named
//! neither the member nor why (E59, a fleet finding, 2026-10-30). Now the batch is cut to the
//! members before it: that prefix is checked again as its own tree and lands as one batch, and
//! the journal names the member and the reason. The member keeps its place for its own turn.
//!
//! The guard to mutate: `join_or_cut` (`src/publish.rs`); give every claim back at the first
//! member that cannot join and the head lands alone.

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::PathBuf;

use support::PRODUCER;
use support::queued::{Check, host, ran_for};

const CONFIG: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\nbatch = { min = 2, max = 8 }\n";

#[test]
fn a_member_resubmitted_while_its_batch_is_checked_cuts_the_batch() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let executor = held.executor("exec", PRODUCER);
    let one = held.queue("one", "src");
    let two = held.queue("two", "src");
    let three = held.queue("three", "src");
    let runner = held
        .test_box
        .bay_process(&executor)
        .args(["--json", "runner", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // The batch of three is checked; while it is, three's owner sends a new revision.
    held.wait_held();
    let started = held.test_box.bay_ok(&held.repo, &["start", &three]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::write(view.join("src/three.rs"), "fn three() { let _ = 3; }\n").unwrap();
    held.test_box.bay_ok(&view, &["merge"]);
    held.go();
    let output = runner.wait_with_output().unwrap();
    let journal = String::from_utf8_lossy(&output.stderr).into_owned();
    let cut = journal
        .lines()
        .find(|line| line.contains("batch of 3 cut to 2"))
        .unwrap_or_else(|| panic!("no cut batch: {journal}"));
    assert!(
        cut.contains(&format!("{three} could not join (")),
        "the cut names three and why: {cut}"
    );
    let pass: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stdout)));
    let batch = &ran_for(&pass, &one)["detail"]["batch"];
    let members: Vec<&str> = batch["members"]
        .as_array()
        .unwrap_or_else(|| panic!("one landed in no batch: {pass}\n{journal}"))
        .iter()
        .map(|member| member["change"].as_str().unwrap())
        .collect();
    assert_eq!(members, [one.as_str(), two.as_str()], "{batch}");
    assert_eq!(batch["stopped"], "not_joined", "{batch}");
    let checked: Vec<u64> = batch["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["members"].as_u64().unwrap())
        .collect();
    assert_eq!(
        checked,
        [3, 2],
        "the batch of three, then one and two as their own tree: {batch}"
    );
    // Three keeps its place and lands on its own turn, as its new revision.
    let landed = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        [&one, &two, &three].iter().all(|id| landed.contains(id)),
        "{landed:?}\n{journal}"
    );
}

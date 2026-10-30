//! G581. A batch passes over a place whose operation this runner cannot read, and names it.
//!
//! A runner reads each place of the line from its change chain. Until 0.6.7 the first place it
//! could not read (a chain no fetch completed, an operation the chain does not hold yet) ended
//! the batch and the line checked ahead, and the journal said only `stopped: unreadable` (a fleet
//! finding, 2026-10-30: a batch of 11 ended so, and nobody could say at which place; 9 of 89 batch
//! decisions on two runners in one day). Now that place is passed over, as a member that conflicts
//! is, keeps its place for its own turn, and the batch decision names its change.
//!
//! The guard to mutate: the loop in `throughput::read_line` (`src/throughput.rs`); end it at the
//! first place it cannot read and the members behind that place land alone.

// Each case runs shell scripts (the check adapter, the relay), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use support::PRODUCER;
use support::queued::{Check, host};

const CONFIG: &str = "[runner]\nlease_s = 30\nheartbeat_s = 1\nbatch = { min = 2, max = 8 }\n";

#[test]
fn a_place_no_fetch_can_read_is_passed_over_and_named() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    // The executor's clone holds no chain yet, so its pass fetches each one.
    let executor = held.executor("exec", PRODUCER);
    let one = held.queue("one", "src");
    let two = held.queue("two", "src");
    let three = held.queue("three", "src");
    // The remote loses two's one new blob, so no fetch of its chain can complete.
    let file = held.test_box.path().join("two.rs");
    std::fs::write(&file, "fn two() {}\n").unwrap();
    let blob = held
        .test_box
        .git_ok(&held.repo, &["hash-object", file.to_str().unwrap()]);
    let blob = blob.trim();
    let loose = held
        .remote
        .join("objects")
        .join(&blob[..2])
        .join(&blob[2..]);
    assert!(
        loose.exists(),
        "the pushed blob is loose in the remote: {}",
        loose.display()
    );
    std::fs::remove_file(&loose).unwrap();
    let output = held
        .test_box
        .bay_process(&executor)
        .args(["--json", "runner", "run", "--once"])
        .output()
        .unwrap();
    let journal = String::from_utf8_lossy(&output.stderr).into_owned();
    let landed = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        landed.contains(&one) && landed.contains(&three) && !landed.contains(&two),
        "one and three land, two waits: {landed:?}\n{journal}"
    );
    assert_eq!(
        held.requests().len(),
        1,
        "one check for one and three, as one batch: {journal}"
    );
    let decision = journal
        .lines()
        .find(|line| line.contains(&one) && line.contains("batch of 2 planned"))
        .unwrap_or_else(|| panic!("no batch of one and three: {journal}"));
    assert!(
        decision.ends_with(&format!("passed over, not read here: {two}")),
        "the batch decision names two: {decision}"
    );
}

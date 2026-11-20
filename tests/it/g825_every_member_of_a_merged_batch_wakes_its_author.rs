//! G825 (a fleet finding, 2026-11-20). A runner that merged a batch woke only the author of the
//! batch's head (G307). Each other member's merge went into the runner's journal and nothing
//! more, so an author who had submitted and left heard of their change's merge only by asking.
//! Each member of a merged batch now wakes its author once, as a change that merges alone does.
//!
//! Guard to mutate, in `src/publish.rs`: the `landed_wake` of each member after its merge is
//! recorded in the batch's publish (drop it and only the head's author is woken).

// Each case runs shell scripts (adapters), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use serde_json::Value;
use support::PRODUCER;
use support::queued::{Check, host};

/// The wake adapter appends each request to `$HOME/woken` (an adapter gets only `PATH` and
/// `HOME`) and answers woken.
const WAKE: &str = "#!/bin/sh\ncat >> \"$HOME/woken\"\necho >> \"$HOME/woken\"\nprintf '{\"schema\":\"bay.wake-result/1\",\"result\":\"woken\",\"reason\":null}'\n";

const CONFIG: &str = "[plugin.wake]\ncommand = \"scripts/bay-wake\"\n\n[runner]\nlease_s = 30\nheartbeat_s = 1\nbatch = { min = 2, max = 8 }\n";

#[test]
fn every_member_of_a_merged_batch_wakes_its_author_once() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n"), ("scripts/bay-wake", WAKE)],
        hold_first: false,
        fails: "false",
    });
    let woken = held.test_box.home.join("woken");
    let executor = held.executor("exec", PRODUCER);

    // GIVEN two changes in the queue, whose authors left
    let ids = [held.queue("one", "src"), held.queue("two", "src")];

    // WHEN a runner merges them as one batch
    let run = || {
        let output = held
            .test_box
            .bay_process(&executor)
            .args(["--json", "runner", "run", "--once"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    let journal = run();
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        ids.iter().all(|id| on_main.contains(id)),
        "both merge: {on_main:?}\n{journal}"
    );
    assert_eq!(
        held.requests().len(),
        1,
        "one check, as one batch: {journal}"
    );

    // THEN each author is woken once, for their own change's merge
    let read = || -> Vec<Value> {
        std::fs::read_to_string(&woken)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    let asked = read();
    for id in &ids {
        let wakes = asked
            .iter()
            .filter(|request| request["change"] == id.as_str() && request["decision"] == "merged")
            .count();
        assert_eq!(
            wakes, 1,
            "{id} was woken {wakes} times: {asked:?}\n{journal}"
        );
    }

    // AND a later pass wakes no one again
    let again = run();
    assert_eq!(read().len(), asked.len(), "{again}");
}

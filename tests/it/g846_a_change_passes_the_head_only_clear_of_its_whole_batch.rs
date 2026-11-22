//! G846 (a fleet finding, 2026-11-21: a code change passed a head on notes that no check of the
//! code change reads, but the head's runner checked it in a batch of nine that held code changes
//! behind the passer; the passer's landing stopped that batch's 18-minute check, which ran again,
//! and the next head was passed the same way). A change runs from its own place past a head only
//! when it is clear of what the head's whole check reads: the batch its holder's lease names
//! (G845), not the head's own paths alone. Otherwise it waits: in the batch, when the batch's
//! check reads it (G852), or right after the batch.
//!
//! Guard to mutate: in `src/publish.rs` `passes_unread`, the chain that adds the revisions of
//! `batch_behind` to the entry's own (runner B runs the change from its own place while the
//! batch is checked).

#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::PRODUCER;
use support::queued::{Check, Held, OTHER_PRODUCER, finished, host, ran_for};

const CONFIG: &str =
    "[runner]\nlease_s = 30\nheartbeat_s = 1\nahead = 4\nbatch = { min = 2, max = 4 }\n";

/// The policy: check `code` on `src/**`, and check `notes` on `notes/**`, each on what it
/// observed.
fn policy() -> String {
    let check = |id: &str, glob: &str| {
        format!(
            "\n[[check]]\nid = \"{id}\"\nenvironment = \"local\"\ncommand = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\nexpected_minimum = 1\ntimeout_s = 120\nwhen_paths = [\"{glob}\"]\nfootprint = \"observed\"\n"
        )
    };
    format!(
        "revision = 1\n{}{}",
        check("code", "src/**"),
        check("notes", "notes/**")
    )
}

/// Waits until the host's lease of `change` names `members` as what its holder's check covers.
fn wait_lease_names(held: &Held, change: &str, members: &[String]) {
    let name = format!(
        "{}:{}",
        bay::lease::lease_ref(change),
        bay::lease::LEASE_FILE
    );
    let start = Instant::now();
    loop {
        let shown = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(&held.remote)
            .args(["show", &name])
            .output()
            .unwrap();
        if shown.status.success() {
            let lease: Value = serde_json::from_slice(&shown.stdout).unwrap();
            if lease["batch"] == json!(members) {
                return;
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the lease of {change} never named {members:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_change_clear_of_the_head_but_not_of_its_batch_waits_for_the_batch() {
    // GIVEN a line of a notes change, a code change, and a second code change
    let policy = policy();
    let seed = [("src/a.rs", "fn a() {}\n"), ("notes/a.rs", "fn a() {}\n")];
    let held = host(&Check {
        config: CONFIG,
        policy: Some(&policy),
        seed: &seed,
        hold_first: true,
        fails: "false",
    });
    let one = held.queue("one", "notes");
    let passer = held.queue("passer", "src");
    let member = held.queue("member", "src");
    // AND runner A checks the three as one batch, the first code change in it because the
    // batch's code check reads it (G852), and its check waits
    let a = held.executor("exec-a", PRODUCER);
    let b = held.executor("exec-b", OTHER_PRODUCER);
    let runner_a = held.spawn(&a);
    held.wait_held();
    wait_lease_names(
        &held,
        &one,
        &[
            format!("{one}.1"),
            format!("{passer}.1"),
            format!("{member}.1"),
        ],
    );

    // WHEN runner B makes a pass
    let during = held.pass(&b);

    // THEN the first code change keeps its place, as the change behind it does: no runner
    // runs it past the batch, whose code check reads it
    for id in [&passer, &member] {
        assert_eq!(ran_for(&during, id)["decision"], "queued", "{during}");
    }
    assert_eq!(
        held.test_box.trunk_change_ids(&held.remote),
        Vec::<String>::new()
    );
    // AND it lands in the batch, in its place in the line
    held.go();
    let first = finished(runner_a);
    let mut after = Vec::new();
    for _ in 0..2 {
        after.push(held.pass(&a));
    }
    let landed = held.test_box.trunk_change_ids(&held.remote);
    let at = |id: &String| {
        landed
            .iter()
            .position(|landed| landed == id)
            .unwrap_or_else(|| panic!("{id} did not land: {first} {after:?}"))
    };
    assert!(at(&member) < at(&passer), "{landed:?}");
    assert!(at(&passer) < at(&one), "{landed:?}");
}

//! G939 (BACKLOG 353, a fleet finding, 2026-11-27: over its ref budget for 35 minutes, a store's
//! runners lost 17 compaction pushes to each other's lease. Each took the shard of its rank among
//! the runners it read live, and a runner whose pass outlasted its presence read as gone to the
//! other, so both counted one rank). A runner takes the shards in an order of its own name
//! (`crate::compact::pick`), so which runners it reads live never sends two runners to one shard:
//! two runners that each read the other as gone pass at once, each on the shard its name orders
//! first, and neither loses its lease.
//!
//! Guard to mutate, in `src/compact.rs`: `pick` answering the lowest shard, as the rank rule did
//! for a runner that read no other live: both runners take one shard, and one loses its lease or
//! moves a shard its name does not order first.

#![cfg(unix)]

use crate::support;

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;
use support::shards::{
    TwoRunners, movable, moved, name_shard, pass, remaining, shard, two_runners,
};
use support::{EXECUTOR, TestBox, result_json};

/// An hour on, every presence reads as stale: a runner reads no other runner live.
const GONE: (&str, &str) = ("BAY_TEST_PRESENCE_CLOCK_SKEW_S", "3600");

/// The names of `names` in shard `chosen`.
fn in_shard(names: &[String], chosen: Option<u8>) -> BTreeSet<String> {
    names
        .iter()
        .filter(|name| Some(shard(name)) == chosen)
        .cloned()
        .collect()
}

/// One pass of the runner in `clone`, reading every other runner as gone.
fn pass_alone(test_box: &TestBox, clone: &Path) -> Value {
    let (code, ran) = result_json(test_box.bay(clone).env(GONE.0, GONE.1).args([
        "runner",
        "run",
        "--once",
        "--interval-s",
        "300",
    ]));
    assert_eq!(code, 0, "{ran}");
    ran
}

/// The names the passes of the runners in `clones` moved, run at once, each reading the other as
/// gone; neither reported an error.
fn at_once_alone(test_box: &TestBox, clones: [&Path; 2]) -> [BTreeSet<String>; 2] {
    let children = clones.map(|clone| {
        test_box
            .bay_process(clone)
            .env(GONE.0, GONE.1)
            .args(["runner", "run", "--once", "--interval-s", "300"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    });
    children.map(|child| {
        let out = child.wait_with_output().unwrap();
        let ran: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        let (names, errors) = moved(&ran);
        assert!(
            errors.is_empty(),
            "a pass lost its lease: {errors:?}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        names
    })
}

#[test]
fn runners_that_read_each_other_as_gone_push_different_shards() {
    let test_box = TestBox::new();
    let TwoRunners {
        host,
        runners: [first, second],
        changes,
        ..
    } = two_runners(&test_box, 3, 1);
    // GIVEN a pass of each runner, which cuts every chain and leaves each one's presence
    for runner in [&first, &second] {
        let (_, errors) = moved(&pass(&test_box, runner));
        assert!(errors.is_empty(), "{errors:?}");
    }

    // WHEN the two runners pass at once, each reading the other as gone, while their names order
    // different shards first
    let mut together = 0;
    for round in 0..40 {
        let left = remaining(&test_box, &host, &changes);
        if left.is_empty() {
            break;
        }
        let now = movable(&changes, &left);
        let firsts = [EXECUTOR, "exec-2"].map(|runner| name_shard(&now, runner));
        if firsts[0] == firsts[1] {
            let clone = if round % 2 == 0 { &first } else { &second };
            let (_, errors) = moved(&pass_alone(&test_box, clone));
            assert!(errors.is_empty(), "{errors:?}");
            continue;
        }
        // THEN each moves names of the shard its own name orders first, and neither loses its
        // lease to the other
        let both = at_once_alone(&test_box, [&first, &second]);
        for (taken, chosen) in both.iter().zip(firsts) {
            let own = in_shard(&now, chosen);
            assert!(
                !taken.is_empty() && taken.is_subset(&own),
                "moved {taken:?}, not of shard {chosen:?}: {own:?}"
            );
        }
        together += 1;
    }
    assert!(together > 0, "the runners never passed at once");

    // AND together they move every ref the cut changes left
    assert_eq!(remaining(&test_box, &host, &changes), BTreeSet::new());
}

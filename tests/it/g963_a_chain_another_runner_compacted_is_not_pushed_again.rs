//! G963 (a fleet finding, 2026-11-28: two runners of one repository pushed an empty shard commit
//! about every 40 s since the day before, 1,366 pushes, each moving a name its shard already
//! recorded). Every runner pass syncs the chains, and the sync pushed each chain the remote
//! lacked: a store that still held a chain a runner had compacted pushed it back, the host had no
//! bay hook to refuse it, and the runner compacted it again. A chain the remote lacks whose
//! shard the remote holds and records it was compacted: the sync forgets it here and never
//! pushes it.
//!
//! Guard to mutate, in `src/receipts.rs` `sync_chains_seen`: `compacted` left empty (no forget),
//! and the second runner's pass creates each chain on the host again.

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::collections::BTreeSet;

use support::TestBox;
use support::shards::{TwoRunners, moved, pass, remaining, two_runners};

#[test]
fn a_chain_the_other_runner_compacted_is_forgotten_not_pushed_again() {
    let test_box = TestBox::new();
    let TwoRunners {
        host,
        runners: [first, second],
        changes,
        ..
    } = two_runners(&test_box, 2, 1);
    // GIVEN a host with no bay hook, as a hosting service is
    for key in ["command", "event"] {
        test_box.git_bare(
            &host,
            &["config", "--unset", &format!("hook.bay-scope.{key}")],
        );
    }
    // AND a second runner whose store holds every chain
    test_box.git_ok(
        &second,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/bay/changes/*:refs/bay/changes/*",
        ],
    );
    let chains: BTreeSet<String> = changes.iter().map(|change| change[0].clone()).collect();

    // WHEN the first runner moves every ref the cut changes left into the shards
    for _ in 0..40 {
        if remaining(&test_box, &host, &changes).is_empty() {
            break;
        }
        let (_, errors) = moved(&pass(&test_box, &first));
        assert!(errors.is_empty(), "{errors:?}");
    }
    assert_eq!(remaining(&test_box, &host, &changes), BTreeSet::new());
    // AND the second runner passes
    let (_, errors) = moved(&pass(&test_box, &second));
    assert!(errors.is_empty(), "{errors:?}");

    // THEN the host holds none of those refs again, and the second store holds none of the chains
    assert_eq!(remaining(&test_box, &host, &changes), BTreeSet::new());
    let held = test_box.git_ok(
        &second,
        &["for-each-ref", "--format=%(refname)", "refs/bay/changes/"],
    );
    let held: BTreeSet<String> = held.lines().map(str::to_owned).collect();
    assert_eq!(held.intersection(&chains).count(), 0, "{held:?}");
}

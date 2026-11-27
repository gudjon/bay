//! G925 (plan S4, CL-1: with chains and requests moving too, runners move three times the names
//! they did, so two of them compact at once more often). Two runners whose passes run at once
//! each take a shard of their own, by their rank among the live runners, from the names each may
//! move now, so neither loses its shard's lease to the other, and together they move every ref
//! the cut changes leave.
//!
//! Guard to mutate: in `src/compact.rs`, `pick` answering the first shard (both runners take one
//! shard, and one loses its lease).

#![cfg(unix)]

use crate::support;

use std::path::Path;

use serde_json::Value;
use support::TestBox;
use support::shards::{
    TwoRunners, index, movable, moved, pass, path, rank_shard, remaining, two_runners,
};

/// The names each of two runner passes in `clones`, run at once, moved; none reported an error.
fn at_once(test_box: &TestBox, clones: &[&Path; 2]) -> [std::collections::BTreeSet<String>; 2] {
    let spawn = |clone: &Path| {
        test_box
            .bay_process(clone)
            .args(["runner", "run", "--once", "--interval-s", "300"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let children = [spawn(clones[0]), spawn(clones[1])];
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
fn two_runners_compacting_at_once_keep_their_leases() {
    let test_box = TestBox::new();
    let TwoRunners {
        host,
        runners: [first, second],
        changes,
        ..
    } = two_runners(&test_box, 3, 1);
    // GIVEN a pass that cuts every chain, and a pass of the second runner, so each knows the
    // other is live
    for runner in [&first, &second] {
        let (_, errors) = moved(&pass(&test_box, runner));
        assert!(errors.is_empty(), "{errors:?}");
    }

    // WHEN the two runners pass at once, while the rule gives each a shard of its own
    let mut together = 0;
    for round in 0..40 {
        let left = remaining(&test_box, &host, &changes);
        if left.is_empty() {
            break;
        }
        let now = movable(&changes, &left);
        if rank_shard(&now, 0, 2) == rank_shard(&now, 1, 2) {
            let (_, errors) = moved(&pass(
                &test_box,
                if round % 2 == 0 { &first } else { &second },
            ));
            assert!(errors.is_empty(), "{errors:?}");
            continue;
        }
        // THEN both push, each its own shard, and neither loses its lease
        let [one, two] = at_once(&test_box, &[&first, &second]);
        assert!(!one.is_empty() && !two.is_empty(), "{one:?} {two:?}");
        assert!(one.is_disjoint(&two), "{one:?} {two:?}");
        together += 1;
    }
    assert!(together > 0, "the runners never passed at once");

    // AND every ref the cut changes left is recorded in its shard
    assert!(remaining(&test_box, &host, &changes).is_empty());
    for name in changes.iter().flatten() {
        let at = format!("{}:{}", index(name), path(name));
        let record =
            name.starts_with("refs/bay/changes/") || name.starts_with("refs/bay/requests/");
        let at = if record { format!("{at}/target") } else { at };
        test_box.git_bare(&host, &["cat-file", "-e", &at]);
    }
}

//! G909 (a fleet finding, 2026-11-25: two runners of one remote removed the same finished queue
//! pairs, and the second one's push was refused on every ref, `stale info`, four times in 6 h).
//! A retention push refused because another runner removed some of its pairs first removes the
//! rest, and reports every pair as removed.
//!
//! Guard to mutate: in `src/queue.rs` `prune`, return the first refusal as an error.

use crate::g200b_executor_cost_stays_flat_with_history::{history, queue_refs};

#[test]
fn a_pair_another_runner_removed_first_is_no_retention_fault() {
    let (test_box, remote, first) = history(5);
    let second = test_box.clone_of(&remote, "second");
    let first = bay::repo::Repo::open(&first).unwrap();
    let second = bay::repo::Repo::open(&second).unwrap();
    // GIVEN two runners that read the same finished pairs
    let read_first = bay::queue::Queue::read(&first).unwrap();
    let read_second = bay::queue::Queue::read(&second).unwrap();
    // AND the first removes two of them
    assert_eq!(
        bay::queue::prune(&first, &read_first, &[1, 2]).unwrap(),
        vec![1, 2]
    );

    // WHEN the second removes those two and a third, on what it read before
    let pruned = bay::queue::prune(&second, &read_second, &[1, 2, 3]);

    // THEN the push is no fault, and all three pairs are gone from the remote and the second store
    assert_eq!(pruned, Ok(vec![1, 2, 3]));
    let left = queue_refs(&test_box, &remote);
    for seq in 1..=3 {
        assert!(
            !left.iter().any(|name| name.ends_with(&format!("/{seq}"))),
            "{left:?}"
        );
    }
    // Of the eleven refs of five finished pairs and the open sixth place, the six of three pairs go.
    assert_eq!(left.len(), 5, "{left:?}");
    let local = second
        .git
        .run(&[
            "for-each-ref",
            "--format=%(refname)",
            "refs/bay/queue-done/",
        ])
        .unwrap();
    assert!(
        !local
            .lines()
            .any(|name| name.ends_with("/1") || name.ends_with("/3")),
        "{local}"
    );
}

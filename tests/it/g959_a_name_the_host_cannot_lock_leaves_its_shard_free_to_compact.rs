//! G959 (BACKLOG 366; a fleet finding, 2026-11-27: a hosted remote listed a chain ref it could
//! not lock, "cannot lock ref '<name>': unable to resolve reference", and one runner pushed the
//! same atomic compaction push 85 times in 5 h, so no other name of that shard moved). A name the
//! host refuses to lock is left out of this store's compaction pushes for a day, and its shard
//! moves on without it.
//!
//! Guards to mutate, in `src/compact.rs`: `refuse` writing nothing; `run` passing over no
//! held-out name. Either way the next pass pushes the same refused push again.

#![cfg(unix)]

use crate::support;

use std::collections::BTreeSet;

use support::shards::{
    TwoRunners, movable, moved, name_shard, pass, remaining, shard, two_runners,
};
use support::{EXECUTOR, TestBox};

#[test]
fn a_name_the_host_cannot_lock_is_left_out_and_the_rest_moves() {
    let test_box = TestBox::new();
    let TwoRunners {
        host,
        runners: [first, _],
        changes,
        ..
    } = two_runners(&test_box, 3, 0);
    // GIVEN a pass that cut the chains, and a host that cannot lock one name of the shard the
    // runner moves next
    let ran = pass(&test_box, &first);
    let (_, errors) = moved(&ran);
    assert!(errors.is_empty(), "{errors:?}");
    let before = movable(&changes, &remaining(&test_box, &host, &changes));
    let next = name_shard(&before, EXECUTOR).unwrap();
    let stuck = before
        .iter()
        .find(|name| shard(name) == next)
        .unwrap()
        .clone();
    let refusal = format!(
        "#!/bin/sh\nwhile read old new ref; do if [ \"$ref\" = '{stuck}' ]; then echo \"error: cannot lock ref '{stuck}': unable to resolve reference '{stuck}'\" >&2; exit 1; fi; done\nexit 0\n"
    );
    let script = test_box.path().join("cannot-lock");
    std::fs::write(&script, refusal).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    test_box.git_bare(&host, &["config", "hook.cannot-lock.event", "pre-receive"]);
    test_box.git_bare(
        &host,
        &[
            "config",
            "hook.cannot-lock.command",
            script.to_str().unwrap(),
        ],
    );

    // WHEN the runner passes
    let (taken, errors) = moved(&pass(&test_box, &first));
    // THEN its push is refused once, and the error names the name it leaves out
    assert!(taken.is_empty(), "{taken:?}");
    let said = serde_json::to_string(&errors).unwrap();
    assert!(said.contains("could not lock"), "{said}");
    assert!(said.contains(&stuck), "{said}");

    // AND the next passes move every other name but its change's chain (a chain goes last, G921),
    // and push no refused push again
    let owner = changes
        .iter()
        .find(|change| change.contains(&stuck))
        .unwrap();
    let kept: BTreeSet<String> = [stuck.clone(), owner[0].clone()].into_iter().collect();
    for _ in 0..32 {
        if remaining(&test_box, &host, &changes) == kept {
            break;
        }
        let (_, errors) = moved(&pass(&test_box, &first));
        assert!(errors.is_empty(), "{errors:?}");
    }
    assert_eq!(remaining(&test_box, &host, &changes), kept);
}

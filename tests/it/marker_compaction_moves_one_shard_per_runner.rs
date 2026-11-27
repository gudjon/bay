//! BACKLOG 332 (a fleet finding, 2026-11-26: every runner's compaction push named the same
//! markers, and a push costs its git client about 2 µs a name for each ref the remote holds, so
//! one push of 4,096 names over 60,000 remote refs did not end inside its bound, and every runner
//! raced it). Over its ref budget, a runner's idle pass moves the names of one shard, at most a
//! batch bound by that cost, and live runners take different shards by their rank, so passes of
//! two runners at once both push.
//!
//! Guards to mutate, in `src/compact.rs`: `pick` answering the lowest shard (its unit tests; here,
//! when the lowest shard left is not the one the second runner's name orders first, it moves
//! another);
//! `batch` answering `PUSH_MAX` (its unit test).
//!
//! BACKLOG 339 (a fleet finding, 2026-11-26: 11 of 145 compaction pushes lost their shard's lease,
//! as runners whose lists differed took one place in them), and BACKLOG 353 (two runners that read
//! each other as gone counted one rank among the live runners): a runner takes the shards in an
//! order of its own name, whatever its list holds and whichever runners it reads live (G939).
//!
//! BACKLOG 334 (a fleet finding, 2026-11-26: at 59,000 remote refs a push of one shard's names
//! still took longer than its bound): a push's batch is sized by the time the last one took, read
//! from the runner's record. Guards: `sized` answering `FIRST` (a slow record does not shrink the
//! batch); `run` writing no record (the record keeps the old push).
//!
//! G921: a cut change's chain and request move too, the chain once its change has no other ref
//! left, so a runner's shard is read from the names it may move now.

#![cfg(unix)]

use crate::support;

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;
use support::shards::{
    TwoRunners, movable, moved, name_shard, pass, path, remaining, shard, two_runners,
};
use support::{EXECUTOR, TestBox};

/// The names of `names` in the shard runner `runner` moves.
fn in_runner_shard(names: &[String], runner: &str) -> BTreeSet<String> {
    let chosen = name_shard(names, runner);
    names
        .iter()
        .filter(|name| Some(shard(name)) == chosen)
        .cloned()
        .collect()
}

#[test]
fn live_runners_over_the_budget_move_different_shards() {
    let test_box = TestBox::new();
    let TwoRunners {
        host,
        runners: [first, second],
        changes,
        markers,
        ..
    } = two_runners(&test_box, 3, 0);
    let names: Vec<String> = changes.iter().flatten().cloned().collect();
    let left = || movable(&changes, &remaining(&test_box, &host, &changes));

    // GIVEN the first runner's pass, which cuts the three chains after its compaction, so it
    // moves nothing yet
    let ran = pass(&test_box, &first);
    let (alone, errors) = moved(&ran);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(alone.is_empty(), "{ran}");
    let cuts = ran["detail"]["ran"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["retention_cut"].is_string())
        .count();
    assert_eq!(cuts, 3, "{ran}");

    // WHEN the second runner passes while the first is live
    let before = left();
    let (taken, errors) = moved(&pass(&test_box, &second));
    // THEN it moves the shard its own name orders first
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        taken,
        in_runner_shard(&before, "exec-2"),
        "exec-2 moves the shard its name orders first"
    );
    let before = left();
    let (taken, errors) = moved(&pass(&test_box, &first));
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        taken,
        in_runner_shard(&before, EXECUTOR),
        "exec-1 moves the shard its name orders first"
    );

    // AND passes of both at once both push, each its own shard, when their names order different
    // shards first
    let now = left();
    if name_shard(&now, EXECUTOR) != name_shard(&now, "exec-2") {
        let spawn = |clone: &Path| {
            test_box
                .bay_process(clone)
                .args(["runner", "run", "--once", "--interval-s", "300"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        };
        let (one, two) = (spawn(&first), spawn(&second));
        let mut both = Vec::new();
        for child in [one, two] {
            let out = child.wait_with_output().unwrap();
            let ran: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
            let (names, errors) = moved(&ran);
            assert!(
                errors.is_empty(),
                "{errors:?}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(!names.is_empty(), "{ran}");
            both.push(names);
        }
        assert!(both[0].is_disjoint(&both[1]), "{both:?}");
    }

    // AND further passes move every name, each recorded in its shard
    for round in 0..32 {
        if remaining(&test_box, &host, &changes).is_empty() {
            break;
        }
        let (_, errors) = moved(&pass(
            &test_box,
            if round % 2 == 0 { &first } else { &second },
        ));
        assert!(errors.is_empty(), "{errors:?}");
    }
    assert_eq!(remaining(&test_box, &host, &changes), BTreeSet::new());
    for (name, target) in &markers {
        let record = format!("refs/bay/landed-index/{}:{}", shard(name), path(name));
        assert_eq!(
            test_box.git_bare(&host, &["cat-file", "blob", &record]),
            *target,
            "{name}"
        );
    }
    for name in names
        .iter()
        .filter(|name| !markers.iter().any(|(marker, _)| marker == *name))
    {
        let kept = format!(
            "refs/bay/landed-index/{}:{}/target",
            shard(name),
            path(name)
        );
        test_box.git_bare(&host, &["cat-file", "-e", &kept]);
    }
}

/// The batch a pass's compaction named, if it pushed.
fn batch_of(ran: &Value) -> Option<u64> {
    ran["detail"]["ran"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|entry| entry.get("batch").and_then(Value::as_u64))
}

#[test]
fn a_push_is_sized_by_the_time_the_last_one_took() {
    let test_box = TestBox::new();
    let TwoRunners {
        runners: [first, _],
        ..
    } = two_runners(&test_box, 3, 0);
    let record = first.join(".git").join("bay").join("compaction-push");

    // GIVEN a pass that cuts the three chains, and no push recorded
    pass(&test_box, &first);
    assert!(!record.exists());

    // WHEN the runner passes with no record
    let ran = pass(&test_box, &first);
    // THEN it pushes at most the first batch, and records the names it pushed and the time
    let (taken, errors) = moved(&ran);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(batch_of(&ran), Some(4), "{ran}");
    assert!(!taken.is_empty() && taken.len() <= 4, "{ran}");
    let written = std::fs::read_to_string(&record).unwrap();
    let (names, ms) = written.trim().split_once(' ').unwrap();
    assert_eq!(names, taken.len().to_string(), "{written}");
    assert!(ms.parse::<u64>().is_ok(), "{written}");

    // WHEN the last push moved 2 names in 60 s, past twice the target
    std::fs::write(&record, "2 60000").unwrap();
    let ran = pass(&test_box, &first);
    // THEN the next push moves half of them: one name
    let (taken, errors) = moved(&ran);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(batch_of(&ran), Some(1), "{ran}");
    assert_eq!(taken.len(), 1, "{ran}");
    // AND the record keeps the push before it, so the next can part a push's fixed cost from
    // its cost a name (BACKLOG 336)
    let kept = std::fs::read_to_string(&record).unwrap();
    let kept: Vec<&str> = kept.lines().collect();
    assert_eq!(kept.len(), 2, "{kept:?}");
    assert_eq!(kept[0], "2 60000", "{kept:?}");
    assert!(kept[1].starts_with("1 "), "{kept:?}");
}

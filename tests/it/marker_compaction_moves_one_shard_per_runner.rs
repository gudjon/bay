//! BACKLOG 332 (a fleet finding, 2026-11-26: every runner's compaction push named the same
//! markers, and a push costs its git client about 2 µs a name for each ref the remote holds, so
//! one push of 4,096 names over 60,000 remote refs did not end inside its bound, and every runner
//! raced it). Over its ref budget, a runner's idle pass moves the names of one shard, at most a
//! batch bound by that cost, and live runners take different shards by their rank, so passes of
//! two runners at once both push.
//!
//! Guards to mutate, in `src/compact.rs`: `pick` answering the first shard (the second runner
//! moves the first runner's shard); `batch` answering `PUSH_MAX` (its unit test).
//!
//! BACKLOG 334 (a fleet finding, 2026-11-26: at 59,000 remote refs a push of one shard's names
//! still took longer than its bound): a push's batch is sized by the time the last one took, read
//! from the runner's record. Guards: `sized` answering `FIRST` (a slow record does not shrink the
//! batch); `run` writing no record (the record keeps the old push).

#![cfg(unix)]

use crate::support;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{
    AGENT, EXECUTOR, PRODUCER, REGISTRAR, TestBox, check_files, key_line, passing_result,
    result_json,
};

/// The shard of `name` (ruling R11): the first byte of sha256(name).
fn shard(name: &str) -> u8 {
    use sha2::Digest as _;
    sha2::Sha256::digest(name.as_bytes())[0]
}

/// One pass of the runner in `clone`, live for the next 300 s.
fn pass(test_box: &TestBox, clone: &Path) -> Value {
    let (code, ran) =
        result_json(
            test_box
                .bay(clone)
                .args(["runner", "run", "--once", "--interval-s", "300"]),
        );
    assert_eq!(code, 0, "{ran}");
    ran
}

/// The names a pass moved, and the errors it reported.
fn moved(ran: &Value) -> (BTreeSet<String>, Vec<Value>) {
    let mut names = BTreeSet::new();
    let mut errors = Vec::new();
    for entry in ran["detail"]["ran"].as_array().into_iter().flatten() {
        if entry.get("error").is_some() {
            errors.push(entry.clone());
        }
        for name in entry["compacted"].as_array().into_iter().flatten() {
            names.insert(name.as_str().unwrap().to_owned());
        }
    }
    (names, errors)
}

/// The names of `markers` the host still holds as refs.
fn remaining(test_box: &TestBox, host: &Path, markers: &[String]) -> Vec<String> {
    markers
        .iter()
        .filter(|name| {
            !test_box
                .git_bare(host, &["for-each-ref", "--format=%(refname)", name])
                .is_empty()
        })
        .cloned()
        .collect()
}

/// The names of `names` in the `nth` lowest of their shards.
fn in_nth_shard(names: &[String], nth: usize) -> BTreeSet<String> {
    let shards: BTreeSet<u8> = names.iter().map(|name| shard(name)).collect();
    let chosen = *shards.iter().nth(nth).unwrap();
    names
        .iter()
        .filter(|name| shard(name) == chosen)
        .cloned()
        .collect()
}

/// A host over a ref budget of 1 with three landed changes, two enrolled runners `exec-1` and
/// `exec-2` in their own clones, and the markers of the three changes with their targets.
fn setup(test_box: &TestBox) -> (PathBuf, [PathBuf; 2], Vec<(String, String)>) {
    let host = test_box.path().join("host.git");
    test_box.host_init(&host);
    let repo = test_box.repo_without_remote_any("repo");
    test_box.add_origin(&repo, &host);
    let mut files = check_files("true", &passing_result(1), 1, 60);
    files.push((
        ".bay/config.toml".to_owned(),
        "[retention]\nwindow_s = 0\nref_budget = 1\n".to_owned(),
    ));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &files);
    let keys = test_box.keys();
    // Each runner signs its results under its own producer name, with its own key.
    let producer_2 = test_box.path().join("keys").join("producer-2");
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "producer-2", "-f"])
        .arg(&producer_2)
        .status()
        .unwrap();
    assert!(made.success());
    test_box.enroll_files(
        &repo,
        &keys,
        &[
            ("agent", key_line(AGENT, &keys.agent)),
            (
                "runner",
                key_line(EXECUTOR, &keys.executor) + &key_line("exec-2", &keys.rogue),
            ),
            (
                "producer",
                key_line(PRODUCER, &keys.producer) + &key_line("ci-2", &producer_2),
            ),
            ("registry", key_line(REGISTRAR, &keys.registry)),
        ],
    );
    test_box.use_keys(&repo, &keys);

    let mut markers = Vec::new();
    for n in 1..=3 {
        let started = test_box.bay_ok(&repo, &["start", &format!("SHARD-{n}"), "--paths", "src/"]);
        let id = started["change"].as_str().unwrap().to_owned();
        let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
        let intent = started["detail"]["intent"].as_str().unwrap().to_owned();
        std::fs::write(
            view.join(format!("src/n{n}.rs")),
            format!("fn n{n}() {{}}\n"),
        )
        .unwrap();
        let (code, landed) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
        assert_eq!(code, 0, "{landed}");
        let landed_intent = intent.replace("refs/bay/intent/", "refs/bay/landed-intent/");
        for name in [format!("refs/bay/landed/{id}"), intent, landed_intent] {
            let target = test_box.git_bare(&host, &["rev-parse", &name]);
            markers.push((name, target));
        }
    }

    let first = test_box.clone_of(&host, "runner-1");
    test_box.use_keys(&first, &keys);
    let second = test_box.clone_of(&host, "runner-2");
    test_box.use_keys(&second, &keys);
    test_box.git_ok(
        &second,
        &["config", "bay.runnerKey", keys.rogue.to_str().unwrap()],
    );
    test_box.git_ok(&second, &["config", "bay.runnerName", "exec-2"]);
    test_box.git_ok(&second, &["config", "bay.producerName", "ci-2"]);
    test_box.git_ok(
        &second,
        &["config", "bay.producerKey", producer_2.to_str().unwrap()],
    );
    (host, [first, second], markers)
}

#[test]
fn live_runners_over_the_budget_move_different_shards() {
    let test_box = TestBox::new();
    let (host, [first, second], markers) = setup(&test_box);
    let names: Vec<String> = markers.iter().map(|(name, _)| name.clone()).collect();

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
    let left = remaining(&test_box, &host, &names);
    let (taken, errors) = moved(&pass(&test_box, &second));
    // THEN it moves the second shard (its rank among two), never the first runner's
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        taken,
        in_nth_shard(&left, 1),
        "exec-2 moves the shard of its rank"
    );
    let left = remaining(&test_box, &host, &names);
    let (taken, errors) = moved(&pass(&test_box, &first));
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(
        taken,
        in_nth_shard(&left, 0),
        "exec-1 moves the shard of its rank"
    );

    // AND passes of both at once both push, each its own shard
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

    // AND further passes move every name, each recorded in its shard
    for round in 0..16 {
        if remaining(&test_box, &host, &names).is_empty() {
            break;
        }
        let (_, errors) = moved(&pass(
            &test_box,
            if round % 2 == 0 { &first } else { &second },
        ));
        assert!(errors.is_empty(), "{errors:?}");
    }
    assert_eq!(remaining(&test_box, &host, &names), Vec::<String>::new());
    for (name, target) in &markers {
        let record = format!(
            "refs/bay/landed-index/{}:{}",
            shard(name),
            name.replace('/', "%2F")
        );
        assert_eq!(
            test_box.git_bare(&host, &["cat-file", "blob", &record]),
            *target,
            "{name}"
        );
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
    let (_host, [first, _second], _markers) = setup(&test_box);
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
    assert!(std::fs::read_to_string(&record).unwrap().starts_with("1 "));
}

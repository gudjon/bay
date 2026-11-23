//! G865 (a fleet measurement, 2026-11-23: a batch of three read each member's chain again before
//! its publishing push, and pushed every member's landed record and terminal record after it): a
//! batch's records ride its publishing push, as a single landing's do (G864). Each member's chain
//! goes on the head this process last pushed, so that push stands for the read of each chain
//! before it. A member its author revised while the batch published refuses that push, and no
//! record of it stays on this box; the batch then goes as before, and lands each member once.
//!
//! Guards to mutate: in `src/publish.rs` `batch_with_records`, push no chain in `riding.specs`
//! (the landing push carries no member's chain); in `src/log.rs` `ride`, keep the records after
//! a refused push (the head's landing is recorded twice).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use support::queued::{Check, Held, finished, host, ran_for};
use support::{EXECUTOR, PRODUCER, TestBox, result_json};

const CONFIG: &str = "[runner]\nlease_s = 450\nheartbeat_s = 120\nbatch = { min = 2, max = 8, max_failure_rate = 1.0 }\n";

/// The terminal records of `op_id` with `outcome` in every executor log on `remote`.
fn terminal_records(test_box: &TestBox, remote: &Path, op_id: &str, outcome: &str) -> usize {
    let logs = test_box.git_bare(
        remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/log/"],
    );
    logs.lines()
        .flat_map(|log| {
            test_box
                .git_bare(remote, &["log", "--format=%H", log])
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|commit| {
            let record: Value = serde_json::from_str(
                &test_box.git_bare(remote, &["show", &format!("{commit}:record.json")]),
            )
            .unwrap();
            record["op_id"] == op_id && record["outcome"] == outcome
        })
        .count()
}

/// The change record of `id` on `remote`.
fn chain(test_box: &TestBox, remote: &Path, id: &str) -> Value {
    serde_json::from_str(&test_box.git_bare(
        remote,
        &["show", &format!("refs/bay/changes/{id}:change.json")],
    ))
    .unwrap()
}

/// The pushes a pass traced in `trace`, each as its arguments joined.
fn pushes(trace: &Path) -> Vec<String> {
    std::fs::read_to_string(trace)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let call: Value = serde_json::from_str(line).unwrap();
            let args: Vec<&str> = call["args"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            args.contains(&"push").then(|| args.join(" "))
        })
        .collect()
}

/// Starts change `intent` on `lib/`, submits it and puts it in the queue, as [`Held::queue`]
/// does: its id, and its view, where its author revises it.
fn queue_in_view(held: &Held, intent: &str) -> (String, PathBuf) {
    let started = held
        .test_box
        .bay_ok(&held.repo, &["start", intent, "--paths", "lib/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::create_dir_all(view.join("lib")).unwrap();
    std::fs::write(
        view.join(format!("lib/{intent}.rs")),
        format!("fn {intent}() {{}}\n"),
    )
    .unwrap();
    let (code, submitted) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    let signer = bay::signing::Signer {
        key: held.keys.executor.clone(),
        name: EXECUTOR.to_owned(),
    };
    bay::queue::join(&opened, Some(&signer), &format!("{id}.1"), &id).unwrap();
    (id, view)
}

#[test]
fn a_batch_pushes_its_records_with_trunk() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let exec = held.executor("exec", PRODUCER);
    let ids: Vec<String> = ["b1", "b2", "b3"]
        .into_iter()
        .map(|name| held.queue(name, "src"))
        .collect();

    // WHEN a pass lands the three in one batch
    let trace = held.test_box.path().join("trace.jsonl");
    let (code, ran) = result_json(
        held.test_box
            .bay(&exec)
            .env("BAY_TEST_GIT_TRACE", &trace)
            .args(["runner", "run", "--once"]),
    );
    assert_eq!(code, 0, "{ran}");
    for id in &ids {
        let landed = ran_for(&ran, id);
        assert_eq!(landed["decision"], "merged", "{ran}");
        assert!(
            !landed["detail"]["batch"].is_null(),
            "not in a batch: {ran}"
        );
    }

    // THEN the push that moves trunk carries every member's chain and the log, and no push
    // after it a chain
    let pushes = pushes(&trace);
    let landing = pushes
        .iter()
        .position(|push| push.contains(":refs/heads/main"))
        .unwrap_or_else(|| panic!("no push moved trunk: {pushes:#?}"));
    for id in &ids {
        assert!(
            pushes[landing].contains(&format!(":refs/bay/changes/{id}")),
            "{id}: {pushes:#?}"
        );
    }
    assert!(pushes[landing].contains(":refs/bay/log/"), "{pushes:#?}");
    assert!(
        pushes[landing + 1..]
            .iter()
            .all(|push| !push.contains("refs/bay/changes/")),
        "{pushes:#?}"
    );

    // AND the remote holds each landing on its chain and once in the log
    for id in &ids {
        let record = chain(&held.test_box, &held.remote, id);
        assert_eq!(record["state"], "landed", "{record}");
        assert_eq!(
            terminal_records(
                &held.test_box,
                &held.remote,
                &format!("{id}.1"),
                "published"
            ),
            1,
            "{id}"
        );
    }
}

#[test]
fn a_member_revised_while_its_batch_publishes_lands_once_in_its_newer_revision() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let one = held.queue("one", "src");
    let (two, view) = queue_in_view(&held, "two");

    // GIVEN the batch of one and two held just before its publishing push
    let runner = held.executor("exec", PRODUCER);
    let release = held.test_box.path().join("release");
    let child = held
        .test_box
        .bay_process(&runner)
        .env("BAY_TEST_PAUSE_AT", "before-publish")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["--json", "runner", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = held.test_box.path().join("release.paused");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !paused.exists() {
        assert!(
            Instant::now() < deadline,
            "the batch never reached its push"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // WHEN two's author revises it, and the batch then pushes
    std::fs::write(view.join("lib/later.rs"), "fn later() {}\n").unwrap();
    let (code, revised) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{revised}");
    std::fs::write(&release, "go").unwrap();
    let pass = finished(child);

    // THEN both land, two in its newer revision, as the hook judges a revision a newer request
    // superseded
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        on_main.contains(&one) && on_main.contains(&two),
        "the batch did not land both: {on_main:?} {pass}"
    );
    let tree = held
        .test_box
        .git_bare(&held.remote, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(tree.contains("lib/later.rs"), "{tree}\n{pass}");

    // AND each landing is recorded once, the refused one never, and the runner's chain refs are
    // the remote's
    for (id, op_id) in [(&one, format!("{one}.1")), (&two, format!("{two}.2"))] {
        let record = chain(&held.test_box, &held.remote, id);
        assert_eq!(record["state"], "landed", "{id}: {record}\n{pass}");
        assert_eq!(
            terminal_records(&held.test_box, &held.remote, &op_id, "published"),
            1,
            "{op_id}: {pass}"
        );
        let name = format!("refs/bay/changes/{id}");
        assert_eq!(
            held.test_box.git_ok(&runner, &["rev-parse", &name]),
            held.test_box.git_bare(&held.remote, &["rev-parse", &name]),
            "{id}"
        );
    }
    let refused = format!("{two}.1");
    assert_eq!(
        terminal_records(&held.test_box, &held.remote, &refused, "published"),
        0,
        "{pass}"
    );
}

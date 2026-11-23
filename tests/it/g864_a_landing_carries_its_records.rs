//! G864 (a fleet measurement, 2026-11-23: a landing pushed main and its markers, then its chain's
//! landed record and its terminal record, each push about 5.5 s on a hosted remote, and read the
//! chain once more before the first): a landing's chain record and terminal record ride in its
//! publishing push. The chain goes on the head this process last pushed, so that push stands for
//! the read of the chain before it: a landing the author revised while it published lands
//! nothing, and leaves no record of a landing on either side.
//!
//! Guards to mutate, in `src/publish.rs`: in `land_with_records`, push no chain in `riding.specs`
//! (the revised landing lands its old revision, and the landing push carries no landed record);
//! in `publish_checked`, drop the `still_owner` read after a refusal (the revised landing lands
//! its old revision on the push that follows).

// Each case runs shell scripts (adapters, a wrapper on PATH), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use support::PRODUCER;
use support::queued::{Check, host, ran_for};
use support::{TestBox, check_files, passing_result, result_json};

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

#[test]
fn a_landing_revised_while_it_publishes_lands_nothing() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(
        &repo,
        &[("src/a.rs", "fn a() {}\n")],
        &check_files("true", &passing_result(1), 1, 60),
    );
    let (id, view) = test_box.start_and_edit(&repo);
    let (code, _) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0);

    // GIVEN executor E1 held just before the push that lands revision R1
    let e1 = test_box.clone_of(&remote, "e1");
    test_box.git_ok(&e1, &["config", "bay.producerName", "e1-checks"]);
    let release = test_box.path().join("release-e1");
    let child = test_box
        .bay_process(&e1)
        .env("BAY_TEST_PAUSE_AT", "before-publish")
        .env("BAY_TEST_PAUSE_FILE", &release)
        .args(["--json", "runner", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let paused = test_box.path().join("release-e1.paused");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !paused.exists() {
        assert!(Instant::now() < deadline, "E1 never stalled");
        std::thread::sleep(Duration::from_millis(50));
    }

    // WHEN the author revises the change to R2 and E1 then pushes
    std::fs::write(view.join("src/r2.rs"), "fn r2() {}\n").unwrap();
    let (code, _) = result_json(test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0);
    std::fs::write(&release, "go").unwrap();
    let output = child.wait_with_output().unwrap();
    let ran: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{ran}");
    assert_ne!(ran_for(&ran, &id)["decision"], "merged", "{ran}");

    // THEN R1 lands nothing: no trunk commit, no landed marker, no record of a landing
    assert_eq!(
        test_box.trunk_change_ids(&remote),
        Vec::<String>::new(),
        "{ran}"
    );
    assert_eq!(
        test_box.git_bare(&remote, &["for-each-ref", "refs/bay/landed/"]),
        "",
        "{ran}"
    );
    let record = chain(&test_box, &remote, &id);
    assert_ne!(record["state"], "landed", "{record}");
    assert!(
        record["operations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|op| op["state"] != "published"),
        "{record}"
    );
    let r1 = format!("{id}.1");
    assert_eq!(
        terminal_records(&test_box, &remote, &r1, "published"),
        0,
        "{ran}"
    );
    // E1 keeps no landed record of its own: its chain is the remote's.
    assert_eq!(
        test_box.git_ok(&e1, &["rev-parse", &format!("refs/bay/changes/{id}")]),
        test_box.git_bare(&remote, &["rev-parse", &format!("refs/bay/changes/{id}")]),
    );

    // AND R2 lands on the next pass
    let e2 = test_box.clone_of(&remote, "e2");
    test_box.git_ok(&e2, &["config", "bay.producerName", "e2-checks"]);
    let ran = test_box.bay_ok(&e2, &["runner", "run", "--once"]);
    assert_eq!(ran_for(&ran, &id)["decision"], "merged", "{ran}");
    assert_eq!(test_box.trunk_change_ids(&remote), vec![id.clone()]);
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/r2.rs"]),
        "fn r2() {}"
    );
}

#[test]
fn a_landing_pushes_its_records_with_trunk() {
    let held = host(&Check {
        config: "[runner]\nlease_s = 450\nheartbeat_s = 120\n",
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: false,
        fails: "false",
    });
    let exec = held.executor("exec", PRODUCER);
    let id = held.queue("one", "src");

    // WHEN a pass lands the change
    let trace = held.test_box.path().join("trace.jsonl");
    let (code, ran) = result_json(
        held.test_box
            .bay(&exec)
            .env("BAY_TEST_GIT_TRACE", &trace)
            .args(["runner", "run", "--once"]),
    );
    assert_eq!(code, 0, "{ran}");
    assert_eq!(ran_for(&ran, &id)["decision"], "merged", "{ran}");

    // THEN the push that moves trunk carries the chain and the log, and no push after it the
    // chain
    let pushes: Vec<String> = std::fs::read_to_string(&trace)
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
        .collect();
    let chain_ref = format!("refs/bay/changes/{id}");
    let landing = pushes
        .iter()
        .position(|push| push.contains(":refs/heads/main"))
        .unwrap_or_else(|| panic!("no push moved trunk: {pushes:#?}"));
    assert!(
        pushes[landing].contains(&format!(":{chain_ref}"))
            && pushes[landing].contains(":refs/bay/log/"),
        "{pushes:#?}"
    );
    assert!(
        pushes[landing + 1..]
            .iter()
            .all(|push| !push.contains(&chain_ref)),
        "{pushes:#?}"
    );

    // AND the remote holds the landing on the chain and in the log
    let record = chain(&held.test_box, &held.remote, &id);
    assert_eq!(record["state"], "landed", "{record}");
    assert_eq!(
        terminal_records(
            &held.test_box,
            &held.remote,
            &format!("{id}.1"),
            "published"
        ),
        1
    );
}

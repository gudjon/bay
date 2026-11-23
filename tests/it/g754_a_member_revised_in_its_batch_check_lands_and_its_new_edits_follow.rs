//! G754 (owner ruling 2026-11-16, "freeze batch members", and its follow-up, "auto follow-up";
//! a fleet finding the same day: two checked batches of six were thrown away because an author
//! revised one member during the batch's check). A member its author revises while its batch
//! checks still lands its checked version with the batch: nothing checked is thrown away. The
//! author's next `bay merge` opens the next change of the same intent from the newer version,
//! under the author's own key, and sends it; it lands after, carrying the newer edits. G865: the
//! batch's records, the revised member's too, ride the one push that lands it.
//!
//! Guards to mutate, in `src/publish.rs`: `frozen` (answer `false`: the revised member leaves
//! the batch and it lands nothing), and `follow_up` (answer `None`: the author's next merge is
//! refused as already on main, and the newer edits never go in); in `src/hook.rs`
//! `frozen_member`, take a checked operation only (the push carrying the records is refused, and
//! main moves in a second push).

#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;

use support::queued::{Check, finished, host};
use support::{EXECUTOR, PRODUCER, result_json};

/// The pushes a pass traced in `trace` that move main, each as its arguments joined.
fn main_pushes(trace: &Path) -> Vec<String> {
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
            (args.contains(&"push") && args.iter().any(|arg| arg.ends_with(":refs/heads/main")))
                .then(|| args.join(" "))
        })
        .collect()
}

const CONFIG: &str = "[runner]\nlease_s = 60\nheartbeat_s = 1\nbatch = { min = 2, max = 8, max_failure_rate = 1.0 }\n";

#[test]
fn a_member_revised_in_its_batch_check_lands_and_its_new_edits_follow() {
    let held = host(&Check {
        config: CONFIG,
        policy: None,
        seed: &[("src/a.rs", "fn a() {}\n")],
        hold_first: true,
        fails: "false",
    });
    let one = held.queue("one", "src");
    // The second member is started here, so its author can revise it in its view.
    let started = held
        .test_box
        .bay_ok(&held.repo, &["start", "two", "--paths", "lib/"]);
    let two = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::create_dir_all(view.join("lib")).unwrap();
    std::fs::write(view.join("lib/two.rs"), "fn two() {}\n").unwrap();
    let (code, submitted) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{submitted}");
    let opened = bay::repo::Repo::open(&held.repo).unwrap();
    let signer = bay::signing::Signer {
        key: held.keys.executor.clone(),
        name: EXECUTOR.to_owned(),
    };
    bay::queue::join(&opened, Some(&signer), &format!("{two}.1"), &two).unwrap();

    // GIVEN the batch of one and two in its check
    let runner = held.executor("exec", PRODUCER);
    let trace = held.test_box.path().join("trace.jsonl");
    let pass = held
        .test_box
        .bay_process(&runner)
        .env("BAY_TEST_GIT_TRACE", &trace)
        .args(["--json", "runner", "run", "--once"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    held.wait_held();

    // WHEN two's author revises it meanwhile
    std::fs::write(view.join("lib/later.rs"), "fn later() {}\n").unwrap();
    let (code, revised) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{revised}");
    held.go();
    let pass = finished(pass);

    // THEN both checked members land with the batch; the newer edits are not on main yet
    let on_main = held.test_box.trunk_change_ids(&held.remote);
    assert!(
        on_main.contains(&one) && on_main.contains(&two),
        "the batch did not land every checked member: {on_main:?} {pass}"
    );
    let mains = main_pushes(&trace);
    assert_eq!(
        mains.len(),
        1,
        "main moved in more than one push: {mains:#?}"
    );
    assert!(
        mains[0].contains(&format!("refs/bay/changes/{two}")),
        "the revised member's records did not ride the landing push: {}",
        mains[0]
    );
    let tree = held
        .test_box
        .git_bare(&held.remote, &["ls-tree", "-r", "--name-only", "main"]);
    let chain = held.test_box.git_bare(
        &held.remote,
        &["log", "--format=%s", &format!("refs/bay/changes/{two}")],
    );
    assert!(
        !tree.contains("lib/later.rs"),
        "the newer edits went in with the batch: {tree}\nchain of two:\n{chain}\npass: {pass}"
    );

    // AND the author's next merge opens the next change from the newer version and sends it
    let (code, again) = result_json(held.test_box.bay(&view).arg("merge"));
    assert_eq!(code, 0, "{again}");
    let next = again["detail"]["follow_up"]["change"]
        .as_str()
        .unwrap_or_else(|| panic!("no follow-up change: {again}"))
        .to_owned();
    assert_ne!(next, two, "{again}");
    assert_eq!(
        again["detail"]["follow_up"]["from"],
        two.as_str(),
        "{again}"
    );
    assert!(
        again["say"].as_str().unwrap_or_default().contains(&next),
        "{again}"
    );

    // AND it lands after, carrying the newer edits
    let landed = held.pass(&runner);
    assert!(
        held.test_box.trunk_change_ids(&held.remote).contains(&next),
        "the follow-up did not land: {landed}"
    );
    let tree = held
        .test_box
        .git_bare(&held.remote, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(tree.contains("lib/later.rs"), "{tree}");
}

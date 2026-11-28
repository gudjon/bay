//! G956 (BACKLOG 374, 375; a fleet finding, 2026-11-28: on a machine that held no credential for
//! the store, a runner's assets plugin exited 2 for `exists`, and the change was blocked
//! `ASSET_MISSING` there, a fault of that machine, not of the change; its author sent it again,
//! unchanged, and the refusal held only the condition, so two seats read no why). A plugin that
//! could not answer on one machine leaves the operation to a live runner on another machine, as
//! a check that gave no answer there does (G950). With no such runner it blocks as before, and an
//! unchanged retry repeats the block's why.
//!
//! The plugin is the reference folder store; its `exists` exits 2 in a runner whose clone holds
//! the marker file `.git/nocred` (a variable that the test sets for bay does not reach the
//! plugin). bay runs the plugin from a scratch folder under that clone's `.git`.
//!
//! Guards to mutate, in `src/publish.rs`: `asked_elsewhere` returning `None` (the first case
//! blocks on bad-box), and `retry_blocked` leaving out `blocked_why` (the second case's retry
//! names no why); in `src/assets.rs`: `exists` errors read as `Unanswered::Tree` (the first
//! case blocks).

#![cfg(unix)]

use crate::g340_a_large_file_saved_never_enters_git::{STORE_PLUGIN, trunk_with, video};
use crate::support;

use std::path::{Path, PathBuf};

use serde_json::Value;
use support::{TestBox, check_files, passing_result, result_json};

/// A repository that offloads a file over 1 MiB, with one check, whose plugin cannot ask the
/// store in a runner whose clone holds `.git/nocred`; and its remote.
fn setup() -> (TestBox, PathBuf, PathBuf) {
    let plugin = STORE_PLUGIN.replacen(
        "exists)\n",
        "exists)\n    if [ -e \"${PWD%%/.git/*}/.git/nocred\" ]; then\n        echo 'folder-store: no credential for the store on this machine' >&2\n        exit 2\n    fi\n",
        1,
    );
    assert_ne!(plugin, STORE_PLUGIN, "the plugin is the reference plugin");
    let files: Vec<(String, String)> = check_files("", &passing_result(1), 1, 60)
        .into_iter()
        .map(|(path, text)| {
            if path == ".bay/policy.toml" {
                (path, format!("{text}\n[limits]\nmax_file_mib = 1\n"))
            } else {
                (path, text)
            }
        })
        .collect();
    trunk_with(&plugin, Some("offload"), &files)
}

/// A runner's clone of `remote` named `name` on machine `machine`. On bad-box, its plugin cannot
/// ask the store.
fn runner(test_box: &TestBox, remote: &Path, name: &str, machine: &str) -> PathBuf {
    let clone = test_box.clone_of(remote, name);
    if machine == "bad-box" {
        std::fs::write(clone.join(".git/nocred"), "").unwrap();
    }
    test_box.git_ok(&clone, &["config", "bay.runnerName", name]);
    test_box.git_ok(
        &clone,
        &["config", "bay.producerName", &format!("{name}-checks")],
    );
    test_box.git_ok(&clone, &["config", "bay.machine", machine]);
    clone
}

/// One pass of the runner in `clone`.
fn pass(test_box: &TestBox, clone: &Path) -> Value {
    result_json(
        test_box
            .bay(clone)
            .args(["runner", "run", "--once", "--interval-s", "300"]),
    )
    .1
}

/// A change that adds a file over the limit, in a view of `repo`, submitted and left to the
/// runners. Returns its id and its view.
fn submitted(test_box: &TestBox, repo: &Path) -> (String, PathBuf) {
    let (id, view) = test_box.start_and_edit(repo);
    std::fs::write(view.join("src/clip.mov"), video()).unwrap();
    let (code, queued) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );
    assert_eq!(code, 0, "{queued}");
    (id, view)
}

/// The subjects of change `id`'s chain on `remote`, newest first.
fn chain(test_box: &TestBox, remote: &Path, id: &str) -> Vec<String> {
    test_box
        .git_bare(
            remote,
            &["log", "--format=%s", &format!("refs/bay/changes/{id}")],
        )
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_machine_whose_plugin_cannot_ask_the_store_leaves_the_change_to_another() {
    let (test_box, repo, remote) = setup();
    let bad = runner(&test_box, &remote, "exec-a", "bad-box");
    let good = runner(&test_box, &remote, "exec-b", "good-box");
    // GIVEN a live runner on good-box, and a change that offloads a large file
    pass(&test_box, &good);
    let (id, _view) = submitted(&test_box, &repo);

    // WHEN the runner on bad-box, whose plugin cannot ask the store, takes it
    let first = pass(&test_box, &bad);

    // THEN it waits for another machine, and names this one and the plugin's why
    let row = &first["detail"]["ran"][0];
    assert_eq!(
        row["reason"],
        "CHECK_UNAVAILABLE",
        "{first}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );
    assert_eq!(row["detail"]["machine"], "bad-box", "{first}");
    assert!(
        row["detail"]["why"]
            .as_str()
            .is_some_and(|why| why.contains("no credential for the store")),
        "{first}"
    );
    // AND the runner on good-box merges it, and it was never blocked
    let landed = pass(&test_box, &good);
    assert!(
        test_box.trunk_change_ids(&remote).contains(&id),
        "the change did not land: {landed}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );
    let subjects = chain(&test_box, &remote, &id);
    assert!(
        !subjects
            .iter()
            .any(|subject| subject.starts_with("blocked")),
        "{subjects:#?}"
    );
}

#[test]
fn with_no_other_machine_it_blocks_and_an_unchanged_retry_repeats_why() {
    let (test_box, repo, remote) = setup();
    let bad = runner(&test_box, &remote, "exec-a", "bad-box");
    // GIVEN a change that offloads a large file, and only a runner on bad-box
    let (id, view) = submitted(&test_box, &repo);
    let first = pass(&test_box, &bad);
    let row = &first["detail"]["ran"][0];
    assert_eq!(
        row["reason"],
        "ASSET_MISSING",
        "{first}\nCHAIN {:#?}",
        chain(&test_box, &remote, &id)
    );

    // WHEN its author sends it again, unchanged
    let (_, again) = result_json(
        test_box
            .bay(&view)
            .env("BAY_TEST_LEAVE_TO_RUNNER", "1")
            .arg("merge"),
    );

    // THEN the refusal is the same, and it repeats the plugin's why
    assert_eq!(again["reason"], "ASSET_MISSING", "{again}");
    assert_eq!(again["detail"]["unchanged"], true, "{again}");
    assert!(
        again["detail"]["why"]
            .as_str()
            .is_some_and(|why| why.contains("no credential for the store")),
        "{again}"
    );
}

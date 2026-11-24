//! G884 (a fleet finding, 2026-11-24: a runner blocked a change `ASSET_MISSING` although the
//! store held its bytes. The runner's clone kept its work tree at an older commit, whose assets
//! plugin asked a store the project had left, and the merge ran that copy; the check, which runs
//! main's copy, would have found the bytes). Before a merge, bay asks the store with the
//! plugin's program as main holds it, never the copy in the clone's work tree.
//!
//! The change's own run is held before its claim and ends there (`BAY_TEST_PAUSE_AT`), so the
//! runner takes the change.
//!
//! Guard to mutate: `src/assets.rs` `not_held_at_merge`, run the plugin from the work tree
//! again (`plugin_of(&repo.config, &repo.work_root)`): the runner's old copy answers "not held"
//! and the change is blocked.

#![cfg(unix)]

use crate::g340_a_large_file_saved_never_enters_git::{
    STORE_PLUGIN, sha256_hex, store, trunk_with, video,
};
use crate::support;

use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use support::{check_files, passing_result};

#[test]
fn a_merge_asks_the_store_with_mains_plugin_not_a_work_trees_old_copy() {
    // GIVEN a repository that offloads a file over 1 MiB, with one check, whose first plugin
    // asks a store that holds nothing
    let old = STORE_PLUGIN.replacen(
        "exists)\n",
        "exists)\n    echo 'folder-store: the old store holds nothing' >&2\n    exit 1\n",
        1,
    );
    assert_ne!(old, STORE_PLUGIN, "the old plugin is the reference plugin");
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
    let (test_box, repo, remote) = trunk_with(&old, Some("offload"), &files);
    test_box.git_ok(&repo, &["config", "bay.machine", "dev-box"]);
    // AND a runner's clone on another machine, its work tree at that commit
    let runner = test_box.clone_of(&remote, "runner");
    test_box.git_ok(&runner, &["config", "bay.machine", "runner-box"]);
    test_box.git_ok(&runner, &["config", "bay.runnerName", "exec-r"]);
    // AND main's plugin then moves to the store the project uses
    std::fs::write(repo.join("scripts/assets"), STORE_PLUGIN).unwrap();
    test_box.git_ok(
        &repo,
        &[
            "commit",
            "-q",
            "-m",
            "the plugin asks the new store",
            "scripts/assets",
        ],
    );
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);

    // WHEN a change adds a file over the limit, its own run ends before it claims the change,
    // and the runner takes it
    let (id, view) = test_box.start_and_edit(&repo);
    let bytes = video();
    std::fs::write(view.join("src/clip.mov"), &bytes).unwrap();
    let held = test_box.path().join("never");
    let waiting = test_box
        .bay_process(&view)
        .args(["--json", "merge", "--wait=90"])
        .env("BAY_TEST_PAUSE_AT", "before-claim")
        .env("BAY_TEST_PAUSE_FILE", &held)
        .env("BAY_TEST_PAUSE_TIMEOUT_S", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let paused = test_box.path().join("never.paused");
    let began = Instant::now();
    while !paused.exists() {
        assert!(
            began.elapsed() < Duration::from_secs(60),
            "the own run never came to its claim"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut ran = Value::Null;
    for _ in 0..3 {
        ran = test_box.bay_ok(&runner, &["runner", "run", "--once"]);
        if test_box.trunk_change_ids(&remote).contains(&id) {
            break;
        }
    }
    let out = waiting.wait_with_output().unwrap();
    let waited = String::from_utf8_lossy(&out.stdout);

    // THEN the change merged, its file named in main's manifest, its bytes in the store
    assert!(
        test_box.trunk_change_ids(&remote).contains(&id),
        "the runner did not merge the change: {ran} | {waited}"
    );
    let hex = sha256_hex(&bytes);
    let manifest = test_box.git_bare(&remote, &["show", "main:.bay-assets"]);
    assert!(
        manifest.contains(&format!("sha256:{hex} {} src/clip.mov", bytes.len())),
        "{manifest}"
    );
    assert!(
        store(&test_box).join(&hex).is_file(),
        "the store does not hold the bytes"
    );
    // AND the runner's work tree still holds the old copy: the merge did not run it
    assert_eq!(
        std::fs::read_to_string(runner.join("scripts/assets")).unwrap(),
        old
    );
}

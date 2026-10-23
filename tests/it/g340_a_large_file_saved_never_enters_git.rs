//! G340 (BACKLOG 49 phase 1, owner ruling 2026-10-21: "large files should never enter Git"): a
//! large file saved never enters Git.
//!
//! With `[plugin.assets] command` and `over_limit = "offload"` in `.bay/config.toml`, a new file
//! over the policy's `[limits] max_file_mib` that `bay save` saves goes to the assets plugin
//! (`put`), and the manifest `.bay-assets` names its bytes (`sha256:<hex> <size> <path>`): the
//! save merges with no blob over the limit reachable from main, the plugin's store holds exactly
//! the bytes, and the file stays in the main checkout, which Git then leaves alone. A plugin
//! that fails refuses the save, and nothing merges. Without `over_limit = "offload"` the file is
//! refused `CHANGE_TOO_LARGE`, as before. `bay plugin test assets` holds a plugin to the
//! contract, and a plugin that names other bytes than it was given fails it.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/save.rs` `offload`: return `(all, Vec::new())` at once (the file is refused, or
//!   merges into Git).
//! - `src/assets.rs` `Plugin::put`: drop the `said != id` refusal (a lying plugin passes the kit).
//! - `src/assets.rs` `offload`: drop the `exclude_paths` call (the file shows as untracked).

// Each case runs shell scripts (a plugin), so these run on Unix only.
#![cfg(unix)]

use crate::support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use support::{TestBox, UNCHECKED_POLICY, result_json};

/// The reference plugin bay ships (`examples/assets/folder-store`): each file's bytes in a
/// folder named after the command, here `$HOME/asset-store`.
pub(crate) const STORE_PLUGIN: &str = include_str!("../../examples/assets/folder-store");

/// A plugin that stores nothing and names bytes it was not given.
const LYING_PLUGIN: &str =
    "#!/bin/sh\necho sha256:0000000000000000000000000000000000000000000000000000000000000000\n";

const FAILING_PLUGIN: &str = "#!/bin/sh\necho 'the store is offline' >&2\nexit 1\n";

pub(crate) const MIB: usize = 1024 * 1024;

/// The folder the reference plugin keeps bytes in.
pub(crate) fn store(test_box: &TestBox) -> PathBuf {
    test_box.home.join("asset-store")
}

/// A main checkout whose policy limits a file to 1 MiB and whose config offloads, with `plugin`
/// as `scripts/assets`; `over_limit` is the config's line, or none.
pub(crate) fn trunk(plugin: &str, over_limit: Option<&str>) -> (TestBox, PathBuf, PathBuf) {
    trunk_with(
        plugin,
        over_limit,
        &[(
            ".bay/policy.toml".to_owned(),
            format!("{UNCHECKED_POLICY}\n[limits]\nmax_file_mib = 1\n"),
        )],
    )
}

/// [`trunk`] with `files` (a policy, a check) in place of the unchecked policy.
pub(crate) fn trunk_with(
    plugin: &str,
    over_limit: Option<&str>,
    files: &[(String, String)],
) -> (TestBox, PathBuf, PathBuf) {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    let over = over_limit.map_or_else(String::new, |value| format!("over_limit = \"{value}\"\n"));
    let mut seeded = files.to_vec();
    seeded.push((
        ".bay/config.toml".to_owned(),
        format!(
            "[plugin.assets]\ncommand = \"scripts/assets {}\"\n{over}",
            store(&test_box).display()
        ),
    ));
    seeded.push(("scripts/assets".to_owned(), plugin.to_owned()));
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &seeded);
    std::fs::set_permissions(
        repo.join("scripts/assets"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    test_box.git_ok(&repo, &["add", "scripts/assets"]);
    test_box.git_ok(&repo, &["commit", "-q", "-m", "the plugin runs"]);
    test_box.git_ok(&repo, &["push", "-q", "origin", "main"]);
    (test_box, repo, remote)
}

/// 3 MiB of bytes no compression folds away.
pub(crate) fn video() -> Vec<u8> {
    (0..3 * MIB)
        .map(|n| n.wrapping_mul(2_654_435_761).to_le_bytes()[1])
        .collect()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// The largest blob reachable from the remote's main, in bytes.
fn largest_blob(test_box: &TestBox, remote: &Path) -> u64 {
    let objects = test_box.git_bare(remote, &["rev-list", "--objects", "main"]);
    objects
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|id| test_box.git_bare(remote, &["cat-file", "-t", id]) == "blob")
        .map(|id| {
            test_box
                .git_bare(remote, &["cat-file", "-s", id])
                .parse::<u64>()
                .unwrap()
        })
        .max()
        .unwrap_or(0)
}

#[test]
fn a_file_over_the_limit_is_stored_by_the_plugin_and_named_in_the_manifest() {
    let (test_box, repo, remote) = trunk(STORE_PLUGIN, Some("offload"));
    let bytes = video();
    std::fs::create_dir_all(repo.join("video")).unwrap();
    std::fs::write(repo.join("video/clip.mov"), &bytes).unwrap();
    std::fs::write(repo.join("src/a.rs"), "fn a() { 1 }\n").unwrap();

    let (code, saved) = result_json(test_box.bay(&repo).args(["save", "add the video"]));
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    assert_eq!(
        saved["detail"]["offloaded"],
        serde_json::json!(["video/clip.mov"]),
        "{saved}"
    );
    assert!(
        largest_blob(&test_box, &remote) < MIB as u64,
        "a blob over the limit is in main"
    );
    let hex = sha256_hex(&bytes);
    let manifest = test_box.git_bare(&remote, &["show", "main:.bay-assets"]);
    assert_eq!(
        manifest,
        format!("sha256:{hex} {} video/clip.mov", bytes.len())
    );
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:src/a.rs"]),
        "fn a() { 1 }",
        "the rest of the save merged"
    );
    let stored = std::fs::read(test_box.home.join("asset-store").join(&hex)).unwrap();
    assert!(stored == bytes, "the store does not hold the saved bytes");

    // The file stays, Git leaves it alone, and saving again has nothing to save.
    assert_eq!(std::fs::read(repo.join("video/clip.mov")).unwrap(), bytes);
    assert_eq!(test_box.git_ok(&repo, &["status", "--porcelain"]), "");
    let (code, again) = result_json(test_box.bay(&repo).args(["save", "add the video"]));
    assert_eq!(code, 0, "{again}");
    assert_eq!(again["detail"]["why"], "nothing to save", "{again}");
}

#[test]
fn a_plugin_that_fails_refuses_the_save_and_nothing_merges() {
    let (test_box, repo, remote) = trunk(FAILING_PLUGIN, Some("offload"));
    let before = test_box.git_bare(&remote, &["rev-parse", "main"]);
    std::fs::write(repo.join("clip.mov"), video()).unwrap();
    let (code, refused) = result_json(test_box.bay(&repo).args(["save", "add the video"]));
    assert_ne!(code, 0, "{refused}");
    let why = refused["detail"].as_str().unwrap_or_default();
    assert!(why.contains("the store is offline"), "{refused}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), before);
}

#[test]
fn without_offload_a_file_over_the_limit_is_refused() {
    let (test_box, repo, _remote) = trunk(STORE_PLUGIN, None);
    std::fs::write(repo.join("clip.mov"), video()).unwrap();
    let (code, refused) = result_json(test_box.bay(&repo).args(["save", "add the video"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "CHANGE_TOO_LARGE", "{refused}");
}

/// `bay plugin test assets --json -- <command>`: `--json` before the `--`, which hands the rest
/// to the plugin's command.
fn kit(test_box: &TestBox, repo: &Path, command: &str) -> (i32, serde_json::Value) {
    let out = test_box
        .bay(repo)
        .args(["plugin", "test", "assets", "--json", "--"])
        .args(command.split_whitespace())
        .output()
        .unwrap();
    let result = serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "not JSON ({error}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    (out.status.code().unwrap_or(-1), result)
}

#[test]
fn the_plugin_test_holds_a_plugin_to_the_contract() {
    let (test_box, repo, _remote) = trunk(STORE_PLUGIN, Some("offload"));
    let command = format!("scripts/assets {}", store(&test_box).display());
    let (code, tested) = kit(&test_box, &repo, &command);
    assert_eq!(code, 0, "{tested}");
    assert_eq!(tested["detail"]["passed"], 4, "{tested}");

    std::fs::write(repo.join("scripts/liar"), LYING_PLUGIN).unwrap();
    std::fs::set_permissions(
        repo.join("scripts/liar"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let (code, failed) = kit(&test_box, &repo, "scripts/liar");
    assert_ne!(code, 0, "{failed}");
    let put = failed["detail"]["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "put")
        .unwrap()
        .clone();
    assert_eq!(put["result"], "fail", "{failed}");
}

/// An agent's view: `bay merge` offloads the file before it freezes the draft, so neither main
/// nor the change's chain on the remote holds its bytes.
///
/// Guard to mutate: `src/publish.rs` `freeze`: drop the `offload_view` call (the draft holds
/// the file).
#[test]
fn a_merge_from_a_view_offloads_the_file_too() {
    let (test_box, repo, remote) = trunk(STORE_PLUGIN, Some("offload"));
    let started = test_box.bay_ok(&repo, &["start", "add the video", "--paths", "video/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    let bytes = video();
    std::fs::create_dir_all(view.join("video")).unwrap();
    std::fs::write(view.join("video/clip.mov"), &bytes).unwrap();
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["decision"], "merged", "{merged}");
    assert!(
        largest_blob(&test_box, &remote) < MIB as u64,
        "a blob over the limit is in main"
    );
    no_large_blob_in_chains(&test_box, &remote);
    let manifest = test_box.git_bare(&remote, &["show", "main:.bay-assets"]);
    assert_eq!(
        manifest,
        format!(
            "sha256:{} {} video/clip.mov",
            sha256_hex(&bytes),
            bytes.len()
        )
    );
}

/// No change chain on the remote holds a blob over the limit.
fn no_large_blob_in_chains(test_box: &TestBox, remote: &Path) {
    let chains = test_box.git_bare(
        remote,
        &["for-each-ref", "--format=%(refname)", "refs/bay/changes/"],
    );
    assert!(!chains.is_empty(), "no chain was pushed");
    for chain in chains.lines() {
        let objects = test_box.git_bare(remote, &["rev-list", "--objects", chain]);
        for id in objects
            .lines()
            .filter_map(|line| line.split_whitespace().next())
        {
            if test_box.git_bare(remote, &["cat-file", "-t", id]) == "blob" {
                let size: u64 = test_box
                    .git_bare(remote, &["cat-file", "-s", id])
                    .parse()
                    .unwrap();
                assert!(size < MIB as u64, "{chain} holds a {size}-byte blob");
            }
        }
    }
}

/// The checkpoint the agent hook pushes at a session's end offloads the file as a merge does.
///
/// Guard to mutate: `src/start.rs` `checkpoint_view`: drop the `offload_view` call (the pushed
/// checkpoint holds the file).
#[test]
fn a_checkpoint_of_a_view_offloads_the_file_too() {
    let (test_box, repo, remote) = trunk(STORE_PLUGIN, Some("offload"));
    let started = test_box.bay_ok(&repo, &["start", "add the video", "--paths", "video/"]);
    let id = started["change"].as_str().unwrap().to_owned();
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    std::fs::create_dir_all(view.join("video")).unwrap();
    std::fs::write(view.join("video/clip.mov"), video()).unwrap();
    let checkpointed = test_box.bay_ok(&repo, &["live", "checkpoint", &id]);
    no_large_blob_in_chains(&test_box, &remote);
    let manifest = std::fs::read_to_string(view.join(".bay-assets")).unwrap();
    assert!(
        manifest.ends_with(" video/clip.mov\n"),
        "{manifest} {checkpointed}"
    );
}

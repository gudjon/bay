//! G345 (BACKLOG 49, owner ruling 2026-10-24, "the rest of the asset work"): a large file merges
//! only when its store holds it, and a purge keeps its address.
//!
//! - Durability: at candidate build the runner asks the assets plugin `exists` for each line a
//!   change adds to `.bay-assets`; bytes the store does not hold refuse the merge
//!   `ASSET_MISSING`, so main never names bytes no store keeps.
//! - Purge: a store may drop a file's bytes (a takedown) and keep its address: `get` answers
//!   purged (exit 3). A clone gets every other file and names the purged one; main's history
//!   and `.bay-assets` stay as they were.
//! - Retention: `[assets] keep = [{ paths, days }]` in the policy, validated by bay (a line
//!   of 0 days is `POLICY_INVALID`), applied by the store.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/publish.rs` `asset_missing`: return `Ok(None)` at once (the change merges).
//! - `src/assets.rs` `fill`: take `Got::Purged` as an error (the clone's install fails).
//! - `src/assets.rs` `AssetsPolicy::validate`: drop the `days == 0` refusal.

// Each case runs shell scripts (a plugin), so these run on Unix only.
#![cfg(unix)]

use crate::g340_a_large_file_saved_never_enters_git::{
    STORE_PLUGIN, sha256_hex, store, trunk, trunk_with, video,
};
use crate::support;

use support::{UNCHECKED_POLICY, result_json};

/// A store that says it stored the bytes and keeps none.
const LEAKY_PLUGIN: &str = r#"#!/bin/sh
hash() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }
case "$2" in
  put) echo "sha256:$(hash "$3")" ;;
  exists) exit 1 ;;
  *) exit 2 ;;
esac
"#;

#[test]
fn bytes_the_store_does_not_hold_never_merge() {
    let (test_box, repo, remote) = trunk(LEAKY_PLUGIN, Some("offload"));
    let before = test_box.git_bare(&remote, &["rev-parse", "main"]);
    std::fs::write(repo.join("clip.mov"), video()).unwrap();
    let (code, refused) = result_json(test_box.bay(&repo).args(["save", "add the clip"]));
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["reason"], "ASSET_MISSING", "{refused}");
    assert_eq!(test_box.git_bare(&remote, &["rev-parse", "main"]), before);
}

#[test]
fn a_purged_file_keeps_its_address_and_a_clone_names_it() {
    let (test_box, repo, remote) = trunk(STORE_PLUGIN, Some("offload"));
    let (gone, kept) = (
        video(),
        video().iter().map(|byte| byte ^ 9).collect::<Vec<u8>>(),
    );
    std::fs::create_dir_all(repo.join("video")).unwrap();
    std::fs::write(repo.join("video/leak.mov"), &gone).unwrap();
    std::fs::write(repo.join("video/keep.mov"), &kept).unwrap();
    let (code, saved) = result_json(test_box.bay(&repo).args(["save", "add two clips"]));
    assert_eq!(code, 0, "{saved}");
    let manifest = test_box.git_bare(&remote, &["show", "main:.bay-assets"]);

    // The store's operator purges one file's bytes.
    let purged = std::process::Command::new(repo.join("scripts/assets"))
        .args([
            store(&test_box).to_str().unwrap(),
            "purge",
            &format!("sha256:{}", sha256_hex(&gone)),
        ])
        .output()
        .unwrap();
    assert!(purged.status.success(), "{purged:?}");

    let clone = test_box.clone_of(&remote, "second");
    let installed = test_box.bay_ok(&clone, &["install"]);
    assert_eq!(
        installed["detail"]["assets"],
        serde_json::json!(["video/keep.mov"]),
        "{installed}"
    );
    assert_eq!(
        installed["detail"]["assets_purged"],
        serde_json::json!(["video/leak.mov"]),
        "{installed}"
    );
    assert_eq!(std::fs::read(clone.join("video/keep.mov")).unwrap(), kept);
    assert!(!clone.join("video/leak.mov").exists());
    assert_eq!(
        test_box.git_bare(&remote, &["show", "main:.bay-assets"]),
        manifest,
        "the address is kept"
    );
}

#[test]
fn a_retention_line_of_no_days_is_refused() {
    let policy = |days: u32| {
        vec![(
            ".bay/policy.toml".to_owned(),
            format!(
                "{UNCHECKED_POLICY}\n[limits]\nmax_file_mib = 1\n\n[assets]\nkeep = [{{ paths = \"renders/\", days = {days} }}]\n"
            ),
        )]
    };
    let (test_box, repo, _remote) = trunk_with(STORE_PLUGIN, Some("offload"), &policy(30));
    std::fs::write(repo.join("clip.mov"), video()).unwrap();
    let (code, saved) = result_json(test_box.bay(&repo).args(["save", "add the clip"]));
    assert_eq!(code, 0, "a valid retention line refused the save: {saved}");

    let (test_box, repo, _remote) = trunk_with(STORE_PLUGIN, Some("offload"), &policy(0));
    std::fs::write(repo.join("clip.mov"), video()).unwrap();
    let (code, refused) = result_json(test_box.bay(&repo).args(["save", "add the clip"]));
    assert_ne!(code, 0, "{refused}");
    assert!(refused.to_string().contains("0 days"), "{refused}");
}

//! G342 (BACKLOG 49 phase 2, owner ruling 2026-10-21): a file kept outside Git comes back
//! wherever its tree is used.
//!
//! A large file saved through `[plugin.assets]` (G340) leaves Git, and `.bay-assets` names its
//! bytes. Every place that uses the tree gets them back through the plugin's `get`, hashed
//! before use: another clone at `bay install`, an agent's view whose declared paths cover it
//! (a view over other paths fetches nothing), and a check, which sees the file read-only and
//! with exactly the bytes that were saved. A check whose files cannot be fetched is an
//! `infra_failure`, never a pass.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/install.rs` `run`: drop the `fill_tree` call (the clone has no file).
//! - `src/view.rs` `create`: drop the `fill_tree` call (the view has no file).
//! - `src/check.rs` `prepare`: drop the `fill_check` call (the check reads no file and fails).

// Each case runs shell scripts (a plugin, a check), so these run on Unix only.
#![cfg(unix)]

use crate::g340_a_large_file_saved_never_enters_git::{
    STORE_PLUGIN, sha256_hex, store, trunk, trunk_with, video,
};
use crate::support;

use std::path::PathBuf;

use support::{TestBox, check_files, passing_result, result_json};

/// A trunk that already holds the offloaded `video/clip.mov` (saved from its main checkout);
/// returns the box, the main checkout, the remote and the bytes.
fn saved_video(
    test_box_files: Option<Vec<(String, String)>>,
) -> (TestBox, PathBuf, PathBuf, Vec<u8>) {
    let (test_box, repo, remote) = match test_box_files {
        Some(files) => trunk_with(STORE_PLUGIN, Some("offload"), &files),
        None => trunk(STORE_PLUGIN, Some("offload")),
    };
    let bytes = video();
    std::fs::create_dir_all(repo.join("video")).unwrap();
    std::fs::write(repo.join("video/clip.mov"), &bytes).unwrap();
    let (code, saved) = result_json(test_box.bay(&repo).args(["save", "add the video"]));
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    (test_box, repo, remote, bytes)
}

#[test]
fn another_clone_gets_the_file_at_install() {
    let (test_box, _repo, remote, bytes) = saved_video(None);
    let clone = test_box.clone_of(&remote, "second");
    assert!(!clone.join("video/clip.mov").exists());
    let installed = test_box.bay_ok(&clone, &["install"]);
    assert_eq!(
        installed["detail"]["assets"],
        serde_json::json!(["video/clip.mov"]),
        "{installed}"
    );
    assert_eq!(std::fs::read(clone.join("video/clip.mov")).unwrap(), bytes);
    assert_eq!(test_box.git_ok(&clone, &["status", "--porcelain"]), "");
}

#[test]
fn a_view_gets_the_files_its_paths_cover_and_no_others() {
    let (test_box, repo, _remote, bytes) = saved_video(None);
    let started = test_box.bay_ok(&repo, &["start", "trim the video", "--paths", "video/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    assert_eq!(std::fs::read(view.join("video/clip.mov")).unwrap(), bytes);
    let started = test_box.bay_ok(&repo, &["start", "fix the code", "--paths", "src/"]);
    let other = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    assert!(
        !other.join("video/clip.mov").exists(),
        "a view over src/ fetched the video"
    );
}

#[test]
fn a_check_sees_the_saved_bytes() {
    let hex = sha256_hex(&video());
    let failing = passing_result(1).replace("\\\"pass\\\"", "\\\"product_failure\\\"");
    // The check passes only when the file is there with exactly the saved bytes.
    let before = format!(
        "h=$( (sha256sum video/clip.mov || shasum -a 256 video/clip.mov) 2>/dev/null | cut -d' ' -f1); [ \"$h\" = {hex} ] || {{ {failing}; exit 0; }}"
    );
    let mut files = check_files(&before, &passing_result(1), 1, 60);
    for (path, text) in &mut files {
        if path == ".bay/policy.toml" {
            text.push_str("\n[limits]\nmax_file_mib = 1\n");
        }
    }
    // `saved_video` asserts the save merged: its check passed on the candidate's bytes.
    let (test_box, _repo, remote, _bytes) = saved_video(Some(files));
    assert!(
        test_box
            .git_bare(&remote, &["show", "main:.bay-assets"])
            .contains(&hex),
        "the merge names no bytes"
    );
}

/// The store a test box's plugin writes to holds the saved bytes (a sanity check of the
/// fixture, so a failing case names the right cause).
#[test]
fn the_reference_store_keeps_the_bytes_by_their_hash() {
    let (test_box, _repo, _remote, bytes) = saved_video(None);
    let kept = std::fs::read(store(&test_box).join(sha256_hex(&bytes))).unwrap();
    assert_eq!(kept, bytes);
}

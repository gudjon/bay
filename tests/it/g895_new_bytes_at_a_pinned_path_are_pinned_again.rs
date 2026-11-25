//! G895 (BACKLOG 295, a fleet finding, 2026-11-25: a change that wrote new bytes over four
//! pinned files merged with their old lines, and the store never got the new bytes): new bytes
//! at a pinned path are pinned again.
//!
//! A file the manifest `.bay-assets` names is not tracked by Git and the clone's
//! `info/exclude` names it, so no `git status` lists new bytes written over it. `bay save`
//! and a view's freeze put them and replace the path's line. A file whose bytes did not move is
//! left alone, and bytes an earlier line of the path named, left in a folder that a newer line
//! passed by, never undo that newer line.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/assets.rs` `offload_view`: pass no pins (the view's new bytes keep the old line).
//! - `src/save.rs` `save_in`: pass no pins (the save has nothing to save).
//! - `src/assets/known.rs` `Stat::of`: leave the change time out (a copy that keeps the old
//!   modification time reads as unmoved).
//! - `src/assets.rs` `moved_in`: drop the earlier-line check (old bytes undo the newer line).

// Each case runs shell scripts (a plugin), so these run on Unix only.
#![cfg(unix)]

use crate::g340_a_large_file_saved_never_enters_git::{
    STORE_PLUGIN, sha256_hex, store, trunk, video,
};
use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json};

/// `video()` with every byte changed: new bytes of the same size.
fn recut(seed: u8) -> Vec<u8> {
    video().iter().map(|byte| byte ^ seed).collect()
}

/// The manifest line main holds for `video/clip.mov`.
fn main_line(test_box: &TestBox, remote: &Path) -> String {
    test_box
        .git_bare(remote, &["show", "main:.bay-assets"])
        .lines()
        .find(|line| line.ends_with(" video/clip.mov"))
        .unwrap_or_default()
        .to_owned()
}

fn line_of(bytes: &[u8]) -> String {
    format!(
        "sha256:{} {} video/clip.mov",
        sha256_hex(bytes),
        bytes.len()
    )
}

/// A trunk whose main checkout saved `video/clip.mov` through the plugin.
fn saved_video() -> (TestBox, PathBuf, PathBuf) {
    let (test_box, repo, remote) = trunk(STORE_PLUGIN, Some("offload"));
    std::fs::create_dir_all(repo.join("video")).unwrap();
    std::fs::write(repo.join("video/clip.mov"), video()).unwrap();
    let (code, saved) = result_json(test_box.bay(&repo).args(["save", "add the video"]));
    assert_eq!(saved["decision"], "merged", "{code} {saved}");
    (test_box, repo, remote)
}

/// Saves an edit of `src/a.rs` from `checkout`, asserting it merged.
fn save_code(test_box: &TestBox, checkout: &Path, body: &str) -> serde_json::Value {
    std::fs::write(checkout.join("src/a.rs"), format!("fn a() {{ {body} }}\n")).unwrap();
    let (code, saved) = result_json(test_box.bay(checkout).args(["save", "edit the code"]));
    assert_eq!(saved["decision"], "merged", "{code} {saved}");
    saved
}

/// The site's repro: a view over the pinned path writes new bytes of the same size there and
/// merges; main's line names the new bytes, and the store holds them.
#[test]
fn new_bytes_in_a_view_merge_with_their_own_line() {
    let (test_box, repo, remote) = saved_video();
    let started = test_box.bay_ok(&repo, &["start", "recut the video", "--paths", "video/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    assert_eq!(std::fs::read(view.join("video/clip.mov")).unwrap(), video());
    let bytes = recut(3);
    std::fs::write(view.join("video/clip.mov"), &bytes).unwrap();
    let (code, merged) = result_json(test_box.bay(&view).args(["merge", "--wait"]));
    assert_eq!(merged["decision"], "merged", "{code} {merged}");
    assert_eq!(main_line(&test_box, &remote), line_of(&bytes));
    let kept = std::fs::read(store(&test_box).join(sha256_hex(&bytes))).unwrap();
    assert_eq!(kept, bytes, "the store does not hold the new bytes");
}

/// A save in the main checkout pins new bytes again, even when the copy kept the file's old
/// modification time; a save of other files leaves an unmoved pinned file's line alone.
#[test]
fn a_save_pins_new_bytes_again_whatever_their_modification_time() {
    let (test_box, repo, remote) = saved_video();
    // A save of other files reads the pinned file and records it, and changes no line.
    save_code(&test_box, &repo, "1");
    assert_eq!(main_line(&test_box, &remote), line_of(&video()));
    let file = repo.join("video/clip.mov");
    let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
    let bytes = recut(5);
    std::fs::write(&file, &bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let (code, saved) = result_json(test_box.bay(&repo).args(["save", "recut the video"]));
    assert_eq!(saved["decision"], "merged", "{code} {saved}");
    assert_eq!(main_line(&test_box, &remote), line_of(&bytes));
}

/// A second clone fetched the first bytes. Main's line then moved to new bytes of the same size,
/// and the clone's save brought it to that line while its folder kept the old bytes (a file of
/// the line's size is kept). Two more saves of other files, one with the clone's record of what
/// it fetched and one without it (a folder from before the record), never put the old bytes back.
#[test]
fn old_bytes_left_in_a_folder_never_undo_a_newer_line() {
    let (test_box, repo, remote) = saved_video();
    let clone = test_box.clone_of(&remote, "second");
    test_box.bay_ok(&clone, &["install"]);
    assert_eq!(
        std::fs::read(clone.join("video/clip.mov")).unwrap(),
        video()
    );
    let bytes = recut(7);
    std::fs::write(repo.join("video/clip.mov"), &bytes).unwrap();
    let (code, saved) = result_json(test_box.bay(&repo).args(["save", "recut the video"]));
    assert_eq!(saved["decision"], "merged", "{code} {saved}");
    assert_eq!(main_line(&test_box, &remote), line_of(&bytes));
    save_code(&test_box, &clone, "2");
    let manifest = std::fs::read_to_string(clone.join(".bay-assets")).unwrap();
    assert!(manifest.contains(&line_of(&bytes)), "{manifest}");
    assert_eq!(
        std::fs::read(clone.join("video/clip.mov")).unwrap(),
        video()
    );
    save_code(&test_box, &clone, "3");
    assert_eq!(
        main_line(&test_box, &remote),
        line_of(&bytes),
        "a clone's save with its record put the old bytes back"
    );
    let record = clone.join(".git/bay-assets-known");
    assert!(record.exists(), "the clone keeps no record");
    std::fs::remove_file(&record).unwrap();
    save_code(&test_box, &clone, "4");
    assert_eq!(
        main_line(&test_box, &remote),
        line_of(&bytes),
        "a clone's save without a record put the old bytes back"
    );
}

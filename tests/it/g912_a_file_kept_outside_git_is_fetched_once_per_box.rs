//! G912 (a fleet finding, 2026-11-25: one project's runners fetched its 97 offloaded files
//! through the assets plugin for every check, about 2 s a file, so each check began 168 to 177 s
//! after its runner took it). A box keeps the bytes a `get` brought back, under their hash, and
//! its next tree takes them from disk: a second check asks the store nothing. Each copy is hashed
//! before it is used: a changed copy is never used, and the store is asked again. A copy a day old
//! is fetched again, so a purge reaches the box.
//!
//! Guards to mutate, in `src/assets.rs`:
//! - `Cache::take`: return `false` at once (the second check asks the store).
//! - `Cache::take`: drop the `identity` comparison (the changed copy reaches the check, which fails).
//! - `fresh`: return `true` (the day-old copy is used, and the store is not asked).

// Each case runs shell scripts (a plugin, a check), so these run on Unix only.
#![cfg(unix)]

use crate::g340_a_large_file_saved_never_enters_git::{
    MIB, STORE_PLUGIN, sha256_hex, trunk_with, video,
};
use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, check_files, passing_result, result_json};

/// The reference plugin, which also writes each identity it is asked to get into `<store>.gets`.
fn counting_plugin() -> String {
    let body = STORE_PLUGIN.strip_prefix("#!/bin/sh\n").unwrap();
    format!("#!/bin/sh\n[ \"${{2:-}}\" = get ] && echo \"$3\" >> \"$1.gets\"\n{body}")
}

/// How many times the store was asked for bytes.
fn gets(test_box: &TestBox) -> usize {
    std::fs::read_to_string(test_box.home.join("asset-store.gets"))
        .map_or(0, |text| text.lines().count())
}

/// Each copy named `hex` in a box cache (`.../bay/assets/cache/<hex>`) under `dir`.
fn copies(dir: &Path, hex: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && !path.is_symlink() {
            found.extend(copies(&path, hex));
        } else if path.file_name().is_some_and(|name| name == hex)
            && path
                .parent()
                .is_some_and(|parent| parent.ends_with("bay/assets/cache"))
        {
            found.push(path);
        }
    }
    found
}

/// Saves an edit of `src/a.rs` (`n`) and asserts it merged: its check passed.
fn save_edit(test_box: &TestBox, repo: &Path, n: u32) {
    std::fs::write(repo.join("src/a.rs"), format!("fn a() {{ {n} }}\n")).unwrap();
    let (code, saved) = result_json(test_box.bay(repo).args(["save", &format!("edit {n}")]));
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
}

#[test]
fn a_file_kept_outside_git_is_fetched_once_per_box() {
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
    let (test_box, repo, _remote) = trunk_with(&counting_plugin(), Some("offload"), &files);
    // GIVEN a file kept outside Git, merged after a check that read its bytes
    std::fs::create_dir_all(repo.join("video")).unwrap();
    std::fs::write(repo.join("video/clip.mov"), video()).unwrap();
    let (code, saved) = result_json(test_box.bay(&repo).args(["save", "add the video"]));
    assert_eq!(code, 0, "{saved}");
    assert_eq!(saved["decision"], "merged", "{saved}");
    let asked = gets(&test_box);
    assert!(asked >= 1, "the check did not ask the store");

    // WHEN the box checks another change
    save_edit(&test_box, &repo, 1);

    // THEN its check took the bytes from the box, and the store was not asked
    assert_eq!(gets(&test_box), asked, "the second check asked the store");
    let kept = copies(test_box.path(), &hex);
    assert!(!kept.is_empty(), "the box keeps no copy");

    // AND a copy changed on disk is never used: the store is asked, and the check passes
    for copy in &kept {
        std::fs::write(copy, vec![0u8; 3 * MIB]).unwrap();
    }
    save_edit(&test_box, &repo, 2);
    assert_eq!(gets(&test_box), asked + 1, "a changed copy was used");

    // AND a copy a day old is fetched again
    for copy in copies(test_box.path(), &hex) {
        let status = std::process::Command::new("touch")
            .args(["-t", "202001010000"])
            .arg(&copy)
            .status()
            .unwrap();
        assert!(status.success());
    }
    save_edit(&test_box, &repo, 3);
    assert_eq!(gets(&test_box), asked + 2, "a day-old copy was used");
}

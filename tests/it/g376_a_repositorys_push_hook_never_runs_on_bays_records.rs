//! G376 (a fleet finding, 2026-10-25: `bay start` failed with "push … exceeded 30s" and
//! "sort: write failed: Broken pipe"). A repository's push hook never runs on bay's records.
//!
//! A repository's `pre-push` hook guards its branches. bay pushes its own records (changes,
//! claims, notes, presence) under `refs/bay/` and `refs/notes/bay/`, so a push of those alone
//! skips the hook, and a hook that is slow or fails on them never breaks a start.
//!
//! Guard to mutate: `src/git.rs` `as_formatted`: drop the `--no-verify` arm (the start fails).

// The hook is a shell script, so this runs on Unix only.
#![cfg(unix)]

use crate::support;

use std::os::unix::fs::PermissionsExt as _;

use support::{TestBox, result_json, unchecked_files};

#[test]
fn a_failing_push_hook_never_breaks_a_start() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let hooks = repo.join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-push");
    std::fs::write(&hook, "#!/bin/sh\necho refused by the hook >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let (code, started) =
        result_json(
            test_box
                .bay(&repo)
                .args(["start", "CARD-hooked", "--paths", "src/"]),
        );
    assert_eq!(code, 0, "the hook ran on bay's records: {started}");
    assert_eq!(started["decision"], "ok", "{started}");
}

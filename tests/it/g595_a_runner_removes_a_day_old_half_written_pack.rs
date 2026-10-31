//! G595 (BACKLOG 150; a fleet finding, 2026-10-30: one runner's clone held 244 half-written
//! packs, 591 MB). Git's index-pack writes a fetched pack as `objects/pack/tmp_pack_*` and
//! leaves it when the fetch fails or is stopped at its time bound, whatever the signal. A
//! runner's pass removes each `objects/pack/tmp_*` file of its own repository that nothing has
//! touched for a day, read-only as Git leaves it. A recent one is kept, as are the packs
//! themselves, however old, and the files of every other repository.
//!
//! Guards to mutate: `src/executor.rs` `pass`: drop the `sweep_tmp_packs` call;
//! `sweep_tmp_packs`: drop its `tmp_` prefix test, or its age test.

#![cfg(unix)]

use crate::support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use support::{TestBox, result_json, unchecked_files};

/// A half-written pack file `name` in `repo`'s pack directory, read-only as Git leaves it,
/// last touched `ago` (a `touch -d` date).
fn left(repo: &Path, name: &str, ago: &str) -> PathBuf {
    let file = repo.join("objects/pack").join(name);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "PACK").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
    age(&file, ago);
    file
}

fn age(path: &Path, ago: &str) {
    let status = std::process::Command::new("touch")
        .args(["-d", ago])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn a_pass_removes_a_day_old_half_written_pack_of_its_own_clone_only() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    let remote = test_box.path().join("repo-remote.git");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    let runner = test_box.clone_of(&remote, "runner");
    // A real pack of the clone, made old: it is never a half-written one.
    test_box.git_ok(&runner, &["repack", "-a", "-d", "-q"]);
    let git = runner.join(".git");
    let packs: Vec<PathBuf> = std::fs::read_dir(git.join("objects/pack"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    assert!(!packs.is_empty(), "the clone holds a pack");
    for pack in &packs {
        age(pack, "2 days ago");
    }
    let stale = left(&git, "tmp_pack_aB3dE9", "2 days ago");
    let index = left(&git, "tmp_idx_aB3dE9", "2 days ago");
    let recent = left(&git, "tmp_pack_fetching", "1 hour ago");
    let other = left(&remote, "tmp_pack_other", "2 days ago");
    let agent = left(&repo.join(".git"), "tmp_pack_agent", "2 days ago");

    let (code, pass) = result_json(test_box.bay(&runner).args(["runner", "run", "--once"]));
    assert_eq!(code, 0, "{pass}");
    assert!(
        !stale.exists(),
        "a day-old half-written pack is removed: {pass}"
    );
    assert!(
        !index.exists(),
        "a day-old half-written index is removed: {pass}"
    );
    assert!(recent.exists(), "a pack a fetch writes now is kept: {pass}");
    for pack in &packs {
        assert!(
            pack.exists(),
            "a pack is kept however old: {}",
            pack.display()
        );
    }
    assert!(other.exists(), "the remote's files are kept");
    assert!(agent.exists(), "another clone's files are kept");
    test_box.git_ok(&runner, &["fsck", "--no-progress"]);
}

//! G762 (owner, 2026-11-16: "is this an antipattern?", a store of 7.4 GB in 624 packs, up from
//! 117 a day before: bay fetches often, its fetches start no auto maintenance (G685), and a
//! scheduled incremental repack consolidates slower than they add packs). Past its pack limit, a
//! store a status sweep reads is compacted in the background by one geometric repack, which keeps
//! every object, and none starts again within half an hour.
//!
//! Guard to mutate: in `src/status.rs`, drop the `crate::packs::bound` call after the sweep, and
//! the store keeps its packs.

#![cfg(unix)]

use crate::support;

use std::time::{Duration, Instant};

use support::{TestBox, unchecked_files};

/// Commits `name` in `repo` and writes its new objects as one pack of their own, as a fetch does.
fn commit_as_pack(test_box: &TestBox, repo: &std::path::Path, name: &str) {
    std::fs::write(
        repo.join(format!("src/{name}.rs")),
        format!("fn {name}() {{}}\n"),
    )
    .unwrap();
    test_box.git_ok(repo, &["add", "--all"]);
    test_box.git_ok(repo, &["commit", "-q", "-m", name]);
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg("git rev-list --objects HEAD^..HEAD | git pack-objects -q .git/objects/pack/pack >/dev/null")
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success());
}

/// How many packs the store of `repo` holds.
fn packs(repo: &std::path::Path) -> usize {
    std::fs::read_dir(repo.join(".git/objects/pack"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".pack"))
        .count()
}

#[test]
fn a_store_past_its_pack_limit_is_compacted_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    // GIVEN a store whose own auto maintenance is off (as on a box bay fetches into), with many
    // small packs, as frequent fetches leave
    test_box.git_ok(&repo, &["config", "maintenance.auto", "false"]);
    test_box.git_ok(&repo, &["config", "gc.auto", "0"]);
    for n in 0..8 {
        commit_as_pack(&test_box, &repo, &format!("p{n}"));
    }
    let before = packs(&repo);
    assert!(before > 3, "{before} packs");
    let objects = test_box.git_ok(&repo, &["count-objects", "-v"]);

    // WHEN a status sweep reads it, past a limit of three
    let _ = support::result_json(
        test_box
            .bay(&repo)
            .env("BAY_TEST_PACK_LIMIT", "3")
            .arg("status"),
    );

    // THEN one compaction merges its packs in the background, and keeps every commit
    let deadline = Instant::now() + Duration::from_secs(60);
    while packs(&repo) > 3 {
        assert!(Instant::now() < deadline, "still {} packs", packs(&repo));
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(repo.join(".git/bay/repack.stamp").exists());
    test_box.git_ok(&repo, &["fsck", "--connectivity-only"]);
    let log = test_box.git_ok(&repo, &["log", "--format=%s"]);
    assert!(
        log.contains("p0") && log.contains("p7"),
        "{log} / {objects}"
    );

    // AND another sweep within the half hour starts none, though packs grew again
    for n in 8..14 {
        commit_as_pack(&test_box, &repo, &format!("p{n}"));
    }
    let grown = packs(&repo);
    let _ = support::result_json(
        test_box
            .bay(&repo)
            .env("BAY_TEST_PACK_LIMIT", "3")
            .env("BAY_TEST_SWEPT_S", "0")
            .arg("status"),
    );
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        packs(&repo),
        grown,
        "a second compaction started within the half hour"
    );
}

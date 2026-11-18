//! G782 (a measurement, 2026-11-18: one small fetch into a store of 120,000 refs took 28.7 s with
//! the refs loose, 1.6 s with them packed). Every fetch reads each local ref first, and bay's
//! fetches start no auto maintenance (G685), so nothing packed the refs they wrote. Past its
//! loose-ref limit, a store a status sweep reads has its refs packed in the background, every
//! ref kept, and no packing starts again within half an hour.
//!
//! Guard to mutate: in `src/packs.rs` `bound`, drop the `refs_bound` call, and the refs stay
//! loose.

#![cfg(unix)]

use crate::support;

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use support::{TestBox, unchecked_files};

/// How many loose refs the store of `repo` holds.
fn loose(repo: &Path) -> usize {
    let mut seen = 0;
    let mut dirs = vec![repo.join(".git/refs")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            if entry.file_type().unwrap().is_dir() {
                dirs.push(entry.path());
            } else {
                seen += 1;
            }
        }
    }
    seen
}

#[test]
fn a_store_past_its_loose_ref_limit_has_its_refs_packed_once() {
    let test_box = TestBox::new();
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    // A store whose refs are a reftable holds no loose ref: nothing to pack.
    if !repo.join(".git/refs/heads").is_dir()
        || test_box
            .git_ok(&repo, &["rev-parse", "--show-ref-format"])
            .trim()
            == "reftable"
    {
        return;
    }
    // GIVEN a store whose own auto maintenance is off, with many loose refs, as fetches leave
    test_box.git_ok(&repo, &["config", "maintenance.auto", "false"]);
    test_box.git_ok(&repo, &["config", "gc.auto", "0"]);
    let head = test_box.git_ok(&repo, &["rev-parse", "HEAD"]);
    let mut stdin = String::new();
    for n in 0..40 {
        writeln!(stdin, "create refs/bay/test-loose/r{n} {}", head.trim()).unwrap();
    }
    let status = std::process::Command::new("git")
        .args(["update-ref", "--stdin"])
        .current_dir(&repo)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child.stdin.take().unwrap().write_all(stdin.as_bytes())?;
            child.wait()
        })
        .unwrap();
    assert!(status.success());
    let before = loose(&repo);
    assert!(before >= 40, "{before} loose refs");
    let refs = test_box.git_ok(&repo, &["for-each-ref"]);

    // WHEN a status sweep reads it, past a limit of ten
    let _ = support::result_json(
        test_box
            .bay(&repo)
            .env("BAY_TEST_LOOSE_LIMIT", "10")
            .arg("status"),
    );

    // THEN its refs are packed in the background, every ref kept
    let deadline = Instant::now() + Duration::from_secs(60);
    while loose(&repo) > 10 {
        assert!(
            Instant::now() < deadline,
            "still {} loose refs",
            loose(&repo)
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(repo.join(".git/bay/pack-refs.stamp").exists());
    assert_eq!(test_box.git_ok(&repo, &["for-each-ref"]), refs);

    // AND another sweep within the half hour packs none, though loose refs grew again
    let mut more = String::new();
    for n in 40..80 {
        writeln!(more, "create refs/bay/test-loose/r{n} {}", head.trim()).unwrap();
    }
    let status = std::process::Command::new("git")
        .args(["update-ref", "--stdin"])
        .current_dir(&repo)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child.stdin.take().unwrap().write_all(more.as_bytes())?;
            child.wait()
        })
        .unwrap();
    assert!(status.success());
    let grown = loose(&repo);
    let _ = support::result_json(
        test_box
            .bay(&repo)
            .env("BAY_TEST_LOOSE_LIMIT", "10")
            .env("BAY_TEST_SWEPT_S", "0")
            .arg("status"),
    );
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        loose(&repo),
        grown,
        "a second packing started within the half hour"
    );
}

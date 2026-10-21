//! G265 (N-13). A box below its disk floor starts nothing.
//!
//! The floor is the box's, never the repository's: Git config `bay.minFreeMiB`, global or for
//! one clone. While the filesystem the views are made on has less free space than the floor,
//! `bay start` refuses `DISK_LOW` (blocked, exit 10) with `free_mib`, `floor_mib` and the
//! path in its detail, and creates no change and no view. A floor that is not a whole number
//! is a failure, never no floor; one the filesystem is above starts as before. Materializing a
//! check's tree refuses the same way (`crate::materialize`'s unit test). `bay doctor` names a
//! `gc.log` in the repository's common dir, left when a background `git gc --auto` failed, as a
//! degraded `gc-log` row.
//!
//! Guards to mutate: `crate::disk::below_floor`'s `Ok((free_mib < floor_mib).then(|| Low {`
//! (make it `Ok(None)`: the start is not refused), `crate::start::start`'s
//! `match crate::disk::below_floor(&repo.git, &repo.views_dir())` (remove it: the view is
//! made), and `crate::doctor::gc_log_row`'s `if !file.is_file() {` (make it always return: no
//! row).

// The floor is not judged on Windows.
#![cfg(unix)]

use crate::support;

use std::path::{Path, PathBuf};

use support::{TestBox, result_json, row, unchecked_files};

/// An exbibyte, in mebibytes: a floor no test machine's disk is above.
const ABSURD_FLOOR: u64 = 1 << 40;

fn seeded(test_box: &TestBox) -> PathBuf {
    let repo = test_box.repo_any("repo");
    test_box.seed_with_check(&repo, &[("src/a.rs", "fn a() {}\n")], &unchecked_files());
    repo
}

/// The views this repository holds.
fn views(repo: &Path) -> usize {
    std::fs::read_dir(repo.join(".git/bay/views")).map_or(0, Iterator::count)
}

fn chains(test_box: &TestBox, repo: &Path) -> usize {
    test_box
        .git_ok(
            repo,
            &["for-each-ref", "--format=%(refname)", "refs/bay/changes/"],
        )
        .lines()
        .count()
}

#[test]
fn a_start_below_the_floor_refuses_disk_low_and_makes_no_view() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    test_box.git_ok(
        &repo,
        &["config", "bay.minFreeMiB", &ABSURD_FLOOR.to_string()],
    );

    let (code, refused) = result_json(
        test_box
            .bay(&repo)
            .args(["start", "CARD-1", "--paths", "src/"]),
    );
    assert_eq!(code, 10, "{refused}");
    assert_eq!(refused["decision"], "blocked", "{refused}");
    assert_eq!(refused["reason"], "DISK_LOW", "{refused}");
    assert_eq!(refused["allowed"], serde_json::json!(["read"]), "{refused}");
    assert_eq!(refused["retry_when"], "free_space_above_floor", "{refused}");
    let detail = &refused["detail"];
    assert_eq!(detail["floor_mib"], ABSURD_FLOOR, "{refused}");
    let free = detail["free_mib"]
        .as_u64()
        .unwrap_or_else(|| panic!("{refused}"));
    assert!(free < ABSURD_FLOOR, "{refused}");
    assert!(
        detail["path"].as_str().is_some_and(|path| !path.is_empty()),
        "{refused}"
    );
    // Nothing was created: no change, no claim, no view.
    assert_eq!(chains(&test_box, &repo), 0);
    assert_eq!(views(&repo), 0);

    // A floor the filesystem is above starts as before.
    test_box.git_ok(&repo, &["config", "bay.minFreeMiB", "0"]);
    let started = test_box.bay_ok(&repo, &["start", "CARD-1", "--paths", "src/"]);
    let view = PathBuf::from(started["detail"]["view"].as_str().unwrap());
    assert!(view.join("src/a.rs").is_file(), "{started}");
    assert!(views(&repo) > 0, "{started}");
}

#[test]
fn a_floor_that_is_not_a_number_is_a_failure() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    test_box.git_ok(&repo, &["config", "bay.minFreeMiB", "plenty"]);
    let (code, failed) = result_json(
        test_box
            .bay(&repo)
            .args(["start", "CARD-1", "--paths", "src/"]),
    );
    assert_eq!(code, 20, "{failed}");
    assert!(
        failed["detail"]
            .as_str()
            .is_some_and(|why| why.contains("bay.minFreeMiB")),
        "{failed}"
    );
    assert_eq!(chains(&test_box, &repo), 0);
}

#[test]
fn doctor_names_a_gc_log_that_blocks_auto_gc() {
    let test_box = TestBox::new();
    let repo = seeded(&test_box);
    let (_, before) = result_json(test_box.bay(&repo).arg("doctor"));
    let rows = before["detail"]["rows"].as_array().unwrap();
    assert!(!rows.iter().any(|row| row["name"] == "gc-log"), "{before}");

    std::fs::write(
        repo.join(".git/gc.log"),
        "warning: There are too many unreachable loose objects\n",
    )
    .unwrap();
    let (_, after) = result_json(test_box.bay(&repo).arg("doctor"));
    let gc = row(&after, "gc-log");
    assert_eq!(gc["state"], "degraded", "{gc}");
    assert!(
        gc["file"]
            .as_str()
            .is_some_and(|file| file.ends_with("gc.log")),
        "{gc}"
    );
    // Advisory: the row alone turns no other row red, and names no reason.
    assert_eq!(before["reason"], after["reason"], "{after}");
}

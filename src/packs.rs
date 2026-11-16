//! G762 (owner, 2026-11-16: "is this an antipattern?", a store of 7.4 GB in 624 packs, up from
//! 117 a day before). bay fetches its records far more often than a person, and each fetch that
//! brings more than a few objects writes a pack; its fetches start no auto maintenance (G685: a
//! detached repack per fetch once ran 556 git processes on one box), and a scheduled incremental
//! repack consolidates far slower than such fetches add packs. So a store bay fetches into grows
//! packs without bound, and every object lookup reads more pack indexes.
//!
//! Past [`PACK_LIMIT`] packs, the runner's pass and the status sweep start one
//! `git repack -d --geometric=2 --write-midx`: it merges only the small packs, so it stays cheap
//! and keeps the count near the logarithm of the store's size. One starts at most every
//! [`EVERY_S`] per store (a stamp in `bay/repack.stamp`), detached and at low priority, never in a
//! command's path; it keeps every object.

use std::path::Path;
use std::process::{Command, Stdio};

/// How many packs a store holds before bay compacts it.
pub const PACK_LIMIT: usize = 50;

/// The least time between two compactions of one store, in seconds.
const EVERY_S: u64 = 1800;

/// [`PACK_LIMIT`]; a test build honours `BAY_TEST_PACK_LIMIT`.
fn limit() -> usize {
    if crate::version::TEST_HOOKS
        && let Some(limit) = std::env::var("BAY_TEST_PACK_LIMIT")
            .ok()
            .and_then(|text| text.parse().ok())
    {
        return limit;
    }
    PACK_LIMIT
}

/// How many pack files the store at `common` holds, its half-written ones left out.
#[must_use]
pub fn count(common: &Path) -> usize {
    std::fs::read_dir(common.join("objects").join("pack")).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".pack") && !name.starts_with("tmp_")
            })
            .count()
    })
}

/// Starts one compaction of the store at `common` when it holds more than [`limit`] packs and
/// none started within [`EVERY_S`]. Best effort: any fault starts nothing.
pub fn bound(common: &Path) {
    if count(common) <= limit() {
        return;
    }
    let bay = common.join("bay");
    let stamp = bay.join("repack.stamp");
    let now = crate::live::unix_now();
    let recent = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .is_some_and(|at| now.saturating_sub(at) < EVERY_S);
    if recent || std::fs::create_dir_all(&bay).is_err() {
        return;
    }
    // The stamp is written before the start, so a second process that reads it starts none.
    let tmp = stamp.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, now.to_string()).is_err()
        || crate::durable::rename(&tmp, &stamp).is_err()
    {
        return;
    }
    let git_dir = common.display().to_string();
    let repack = [
        "--git-dir",
        git_dir.as_str(),
        "repack",
        "-d",
        "-q",
        "--geometric=2",
        "--write-midx",
    ];
    let mut cmd = if cfg!(unix) {
        let mut nice = Command::new("nice");
        nice.args(["-n", "10", "git"]).args(repack);
        nice
    } else {
        let mut git = Command::new("git");
        git.args(repack);
        git
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::platform::detach(&mut cmd);
    if let Ok(child) = cmd.spawn() {
        crate::platform::reap(child);
    }
}

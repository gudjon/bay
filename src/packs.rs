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
/// none started within [`EVERY_S`], and one packing of its refs past [`LOOSE_LIMIT`] loose refs
/// (G782). Best effort: any fault starts nothing.
pub fn bound(common: &Path) {
    refs_bound(common);
    if count(common) <= limit() {
        return;
    }
    if !claim(common, "repack.stamp") {
        return;
    }
    let git_dir = common.display().to_string();
    start(&[
        "--git-dir",
        git_dir.as_str(),
        "repack",
        "-d",
        "-q",
        "--geometric=2",
        "--write-midx",
    ]);
}

/// G782 (a measurement, 2026-11-18: one small fetch into a store of 120,000 refs took 28.7 s
/// with the refs loose, 1.6 s with them packed and 0.47 s packed with a commit-graph). Every
/// fetch reads each local ref first, and a loose ref is a file of its own. bay's fetches start
/// no auto maintenance (G685), so nothing packed the refs they wrote: a store held thousands
/// loose. Past [`LOOSE_LIMIT`] loose refs, at most every [`EVERY_S`] per store (a stamp in
/// `bay/pack-refs.stamp`, read before any count), one `git pack-refs --all` starts, detached
/// and at low priority; it keeps every ref. A store whose refs are a reftable compacts itself
/// and holds no loose ref.
pub const LOOSE_LIMIT: usize = 1000;

/// [`LOOSE_LIMIT`]; a test build honours `BAY_TEST_LOOSE_LIMIT`.
fn loose_limit() -> usize {
    if crate::version::TEST_HOOKS
        && let Some(limit) = std::env::var("BAY_TEST_LOOSE_LIMIT")
            .ok()
            .and_then(|text| text.parse().ok())
    {
        return limit;
    }
    LOOSE_LIMIT
}

/// How many loose refs the store at `common` holds under `refs/`, counting no further than
/// `most`.
#[must_use]
pub fn loose(common: &Path, most: usize) -> usize {
    let mut seen = 0;
    let mut dirs = vec![common.join("refs")];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => dirs.push(entry.path()),
                Ok(kind)
                    if kind.is_file()
                        && !entry.file_name().to_string_lossy().ends_with(".lock") =>
                {
                    seen += 1;
                    if seen > most {
                        return seen;
                    }
                }
                _ => {}
            }
        }
    }
    seen
}

/// Packs the refs of the store at `common` when they are loose past [`loose_limit`].
fn refs_bound(common: &Path) {
    if recent(common, "pack-refs.stamp") || loose(common, loose_limit()) <= loose_limit() {
        return;
    }
    if !claim(common, "pack-refs.stamp") {
        return;
    }
    let git_dir = common.display().to_string();
    start(&["--git-dir", git_dir.as_str(), "pack-refs", "--all"]);
}

/// Whether the stamp `name` in the store's `bay` folder names a start within [`EVERY_S`].
fn recent(common: &Path, name: &str) -> bool {
    let now = crate::live::unix_now();
    std::fs::read_to_string(common.join("bay").join(name))
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .is_some_and(|at| now.saturating_sub(at) < EVERY_S)
}

/// Takes the stamp `name` for a start now, unless one stands within [`EVERY_S`]. The stamp is
/// written before the start, so a second process that reads it starts none.
fn claim(common: &Path, name: &str) -> bool {
    let bay = common.join("bay");
    if recent(common, name) || std::fs::create_dir_all(&bay).is_err() {
        return false;
    }
    let stamp = bay.join(name);
    let tmp = stamp.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, crate::live::unix_now().to_string()).is_err()
        || crate::durable::rename(&tmp, &stamp).is_err()
    {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

/// Starts `git <args>` detached, at low priority, with no input or output.
fn start(args: &[&str]) {
    let mut cmd = if cfg!(unix) {
        let mut nice = Command::new("nice");
        nice.args(["-n", "10", "git"]).args(args);
        nice
    } else {
        let mut git = Command::new("git");
        git.args(args);
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

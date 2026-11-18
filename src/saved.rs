//! G714 (a measurement, 2026-11-13: on a store of 6,292 change refs and a remote of 59,387 refs,
//! every `bay status` spent 1.5-2.5 s rebuilding one answer from the store, read after read, while
//! nothing in the store had moved). A status listing's answer is saved with the store's state it
//! was built from, and a listing that finds the store as it was answers it again at once.
//!
//! The state is stamped by time, not read: each directory of the ref namespaces bay reads, the
//! packed refs, the reftable's table list, `HEAD`, the receipts and the worktrees. Every writer of
//! a ref, bay or plain Git, writes a loose ref by renaming a file into its directory (which moves
//! that directory's time, and a new directory moves its parent's), rewrites the packed refs, or
//! rewrites the reftable's table list. So a listing that finds every time as it was finds every
//! ref as it was, whoever wrote it, and stats about two thousand paths to know it (no Git, no
//! directory read).
//!
//! An answer is served only while it is young ([`SAVED_MAX_S`]: what the store holds stands still,
//! but a lease, a presence and a talk's due time are judged against the clock), by the bay that
//! saved it, to the actor it was built for, in the checkout it was built in (never a view), and
//! with the sweep record it was built after. It is saved by the sweep that settled the store and
//! by a listing that read the store whole; never one that answered stale, and never a talk accept,
//! which happened once.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::{Value, json};

use crate::result::{BayResult, Freshness};

/// The oldest, in seconds, a saved answer is served.
pub const SAVED_MAX_S: u64 = 120;

/// The ref namespaces a status listing reads, under the common directory's `refs/`.
const NAMESPACES: [&str; 7] = [
    "bay", "notes", "remotes", "heads", "tags", "deploy", "sandbox",
];

/// G715 (a measurement, 2026-11-14: on a store whose agent sessions each fetch the fleet's
/// presence at a turn's start, no listing was ever served its saved answer): the presence refs
/// are not stamped. What a listing judges from them, whether a runner was seen lately, is judged
/// against the clock, and a saved answer is never older than [`SAVED_MAX_S`].
const UNSTAMPED: [&str; 2] = [crate::names::FLEET, crate::names::FLEET_EXECUTOR];

/// G778: the saved answer of a read in the checkout at `root` of the store whose common directory
/// is `common`, as `actor`, of `change` (or the listing): one file each, so a seat's read in its
/// own worktree and a read of one change keep answers of their own.
fn file(common: &Path, root: &Path, actor: &str, change: Option<&str>) -> PathBuf {
    use sha2::{Digest as _, Sha256};
    let mut hash = Sha256::new();
    for part in [root.to_string_lossy().as_ref(), actor, change.unwrap_or("")] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    let digest = hash.finalize();
    let name = crate::agent_settings::hex(&digest[..12]);
    common
        .join("bay")
        .join("status-answers")
        .join(format!("{name}.json"))
}

/// G778: the checkout of `dir` and its store's common directory, found with no Git: a store's own
/// checkout, or a linked worktree or view, whose `.git` file leads to its common directory.
#[must_use]
pub fn checkout(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let dir = dir.canonicalize().ok()?;
    let root = dir.ancestors().find(|root| root.join(".git").exists())?;
    let git = root.join(".git");
    if git.is_dir() {
        return Some((root.to_path_buf(), git));
    }
    let link = std::fs::read_to_string(&git).ok()?;
    let own = root.join(link.trim().strip_prefix("gitdir:")?.trim());
    let common = std::fs::read_to_string(own.join("commondir")).ok()?;
    Some((
        root.to_path_buf(),
        own.join(common.trim()).canonicalize().ok()?,
    ))
}

/// The checkout of `dir` and its Git directory, when the checkout is a store's own (its `.git`
/// a directory, not a view's or a linked worktree's file), found with no Git.
#[must_use]
pub fn own_checkout(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let dir = dir.canonicalize().ok()?;
    let root = dir.ancestors().find(|root| root.join(".git").exists())?;
    let git = root.join(".git");
    git.is_dir().then(|| (root.to_path_buf(), git))
}

/// A path's time in nanoseconds since the epoch, and its size for a file; `null` when absent.
fn mark(path: &Path) -> Value {
    std::fs::metadata(path).map_or(Value::Null, |meta| {
        let nanos = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |time| time.as_nanos());
        let size = if meta.is_dir() { 0 } else { meta.len() };
        json!(format!("{nanos}/{size}"))
    })
}

/// The paths whose times stamp the store at `common`: every directory of [`NAMESPACES`] and of
/// the receipts, read now, and the files that hold refs outside them.
fn paths(common: &Path) -> Vec<String> {
    let mut paths: Vec<String> = [
        "packed-refs",
        "reftable/tables.list",
        "HEAD",
        "worktrees",
        "refs",
    ]
    .iter()
    .map(|path| (*path).to_owned())
    .collect();
    let mut pending: Vec<String> = NAMESPACES
        .iter()
        .map(|space| format!("refs/{space}"))
        .chain(std::iter::once("bay/receipts".to_owned()))
        .collect();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(common.join(&dir)) else {
            // An absent namespace is stamped absent: its making moves `refs`.
            paths.push(dir);
            continue;
        };
        for entry in entries.flatten() {
            let path = format!("{dir}/{}", entry.file_name().to_string_lossy());
            let presence = UNSTAMPED
                .iter()
                .any(|space| space.strip_suffix('/') == Some(&*path));
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) && !presence {
                pending.push(path);
            }
        }
        paths.push(dir);
    }
    paths.sort();
    paths
}

/// The stamp of `paths` in the store at `common`.
fn stamp(common: &Path, paths: &[String]) -> Vec<Value> {
    paths
        .iter()
        .map(|path| {
            if PRESENCE_PARENT.strip_suffix('/') == Some(path.as_str()) {
                held(&common.join(path))
            } else {
                mark(&common.join(path))
            }
        })
        .collect()
}

/// The directory that holds the presence refs ([`UNSTAMPED`]).
const PRESENCE_PARENT: &str = crate::names::BAY;

/// G715: the directory `dir` stamped by what it holds, not by its own time: each entry's name, and
/// each file's mark, but for the presence directories, which a presence fetch prunes away and
/// makes again ([`UNSTAMPED`]).
fn held(dir: &Path) -> Value {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Value::Null;
    };
    let mut rows: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = format!("{PRESENCE_PARENT}{name}/");
            if UNSTAMPED.contains(&path.as_str()) {
                return None;
            }
            Some(if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                name
            } else {
                format!("{name} {}", mark(&entry.path()))
            })
        })
        .collect();
    rows.sort();
    json!(rows)
}

/// G778b: the ref stores whose folders a quick stamp walks, under the common directory, two
/// levels deep: a loose ref is written by a rename into its own folder, which moves that folder's
/// time, so a ref of `refs/bay/talk/<actor>/<id>` moves `refs/bay/talk/<actor>`.
const QUICK_ROOTS: [&str; 5] = [
    "refs/bay",
    "refs/notes",
    "refs/remotes",
    "refs/heads",
    "refs/tags",
];

/// G778b: the quick stamp of the store at `common` and of the views at `views`: the files every
/// ref writer rewrites (`packed-refs`, the reftable's table list, `HEAD`), the ref folders of
/// [`QUICK_ROOTS`] two levels deep but the presence ones ([`UNSTAMPED`]), and each view's own stamp
/// (`crate::view::stamp`: its index, its `HEAD` and its root). A few hundred `stat`s: the cost of
/// knowing a saved answer still stands, where [`stamp`] walks every ref folder for seconds.
fn quick(common: &Path, views: &[String]) -> Value {
    let mut marks: Vec<Value> = ["packed-refs", "reftable/tables.list", "HEAD", "refs"]
        .iter()
        .map(|path| json!([path, mark(&common.join(path))]))
        .collect();
    for root in QUICK_ROOTS {
        let mut dirs = vec![(root.to_owned(), 0)];
        while let Some((dir, depth)) = dirs.pop() {
            marks.push(json!([dir, mark(&common.join(&dir))]));
            if depth == 2 {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(common.join(&dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = format!("{dir}/{}", entry.file_name().to_string_lossy());
                let presence = UNSTAMPED
                    .iter()
                    .any(|space| space.strip_suffix('/') == Some(path.as_str()));
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) && !presence {
                    dirs.push((path, depth + 1));
                }
            }
        }
    }
    marks.sort_by_key(ToString::to_string);
    for view in views {
        marks.push(json!([view, crate::view::stamp(Path::new(view))]));
    }
    Value::from(marks)
}

/// G778b: the views a saved `detail` names, whose own files its answer judged.
fn views_of(detail: &Value) -> Vec<String> {
    let mut views: Vec<String> = detail["changes"]
        .as_array()
        .map(|changes| {
            changes
                .iter()
                .filter_map(|row| row["view"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    views.sort();
    views.dedup();
    views
}

/// The sweep record's time, which a saved answer was built after.
fn swept_at(common: &Path) -> Option<String> {
    crate::sweep::last(common).map(|(at, _)| at)
}

/// The actor a read is made as: its name alone, with no look for its session.
fn actor() -> String {
    crate::change::actor_name()
}

/// What a listing's answer is built from, stamped before it is built: saved by [`Before::save`]
/// only when the store is as it was after the answer is built.
pub struct Before {
    root: PathBuf,
    common: PathBuf,
    change: Option<String>,
    paths: Vec<String>,
    stamp: Vec<Value>,
    swept_at: Option<String>,
    /// G778b: the quick stamp of the store's refs before the answer was built.
    quick: Value,
}

impl Before {
    /// The store of `dir` before an answer of `change` (or the listing) is built in it. A read
    /// that waits for nothing (`stamped` false) is saved for the next such read alone and stamps
    /// nothing: the stamp walks every ref directory, seconds on a store of thousands of changes.
    #[must_use]
    pub fn of(dir: &Path, change: Option<&str>, stamped: bool) -> Option<Self> {
        if off() {
            return None;
        }
        let (root, common) = checkout(dir)?;
        let paths = if stamped { paths(&common) } else { Vec::new() };
        let stamp = stamp(&common, &paths);
        Some(Self {
            root,
            quick: quick(&common, &[]),
            swept_at: swept_at(&common),
            common,
            change: change.map(str::to_owned),
            paths,
            stamp,
        })
    }

    /// Saves `result`, a listing of the store, when nothing it was built from moved while it was
    /// built. A talk accept is never saved: it happened once. A `stale` answer, one read from the
    /// store with no sweep, is saved for `--deadline 0` reads alone (G778): a read that waits
    /// never serves it ([`answer`]).
    pub fn save(self, result: &BayResult, stale: bool) {
        let moved = || {
            stamp(&self.common, &self.paths) != self.stamp
                || swept_at(&self.common) != self.swept_at
        };
        if result.decision != crate::result::Decision::Ok || (!stale && moved()) {
            return;
        }
        let Some(mut detail) = result.detail.clone() else {
            return;
        };
        if let Some(detail) = detail.as_object_mut() {
            detail.remove("talk_accept");
        }
        // G778b: a quick stamp only when no ref moved while the answer was built; the views it
        // names are stamped now, after it judged them.
        let views = views_of(&detail);
        let quick_now = quick(&self.common, &[]);
        let current = (quick_now == self.quick).then(|| quick(&self.common, &views));
        let saved = json!({
            "bay": crate::version::BAY,
            "root": self.root,
            "actor": actor(),
            "swept_at": self.swept_at,
            "saved": crate::live::unix_now(),
            "paths": self.paths,
            "stamp": self.stamp,
            "change": result.change,
            "stale": stale,
            "quick": current,
            "views": views,
            "freshness": result.freshness.as_ref().map(|fresh| json!({
                "trunk": fresh.trunk,
                "fetched_at": fresh.fetched_at,
            })),
            "detail": detail,
        });
        let path = file(&self.common, &self.root, &actor(), self.change.as_deref());
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        replace(&path, &saved.to_string());
    }
}

/// Writes `text` at `path` whole, through a file of this process beside it. Best effort.
fn replace(path: &Path, text: &str) {
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, text).is_ok() {
        let _ = crate::durable::rename(&tmp, path);
    }
}

/// In a test build, `BAY_TEST_SAVED=off` saves no answer and serves none: every listing reads the
/// store whole, for a test of what that read does.
fn off() -> bool {
    crate::version::TEST_HOOKS && std::env::var("BAY_TEST_SAVED").is_ok_and(|value| value == "off")
}

/// The saved answer of a listing at `dir`, when the store is as it was built from: its age in
/// `detail.saved_s`, and how old its view of the remote is now in `detail.stale_s`.
#[must_use]
pub fn answer(dir: &Path) -> Option<BayResult> {
    if off() {
        return None;
    }
    let (root, common) = own_checkout(dir)?;
    let (saved, age) = read(&common, &root, None)?;
    if saved["stale"] == true || !fresh(&common, &saved, age) {
        return None;
    }
    let now = crate::live::unix_now();
    let mut detail = saved["detail"].clone();
    detail["saved_s"] = json!(age);
    if let Some((_, unix)) = crate::sweep::last(&common)
        && detail.get("stale_s").is_some_and(|stale| !stale.is_null())
    {
        detail["stale_s"] = json!(now.saturating_sub(unix));
    }
    let mut result = BayResult::ok(detail);
    if let Some(change) = saved["change"].as_str() {
        result = result.with_change(change.to_owned());
    }
    result.freshness = saved["freshness"].as_object().and_then(|fresh| {
        Some(Freshness {
            trunk: fresh.get("trunk")?.as_str()?.to_owned(),
            fetched_at: fresh.get("fetched_at")?.as_str()?.to_owned(),
        })
    });
    Some(result)
}

/// The answer saved for `change` (or the listing) in the checkout at `root`, as this actor, by
/// this bay, and its age in seconds.
fn read(common: &Path, root: &Path, change: Option<&str>) -> Option<(Value, u64)> {
    let text = std::fs::read_to_string(file(common, root, &actor(), change)).ok()?;
    let saved: Value = serde_json::from_str(&text).ok()?;
    let age = crate::live::unix_now().checked_sub(saved["saved"].as_u64()?)?;
    (saved["bay"] == crate::version::BAY && saved["root"] == json!(root)).then_some((saved, age))
}

/// Whether `saved`, `age` seconds old, stands for the store at `common` as it is now: young, after
/// the same sweep, and every stamped path as it was.
fn fresh(common: &Path, saved: &Value, age: u64) -> bool {
    let Ok(paths) = serde_json::from_value::<Vec<String>>(saved["paths"].clone()) else {
        return false;
    };
    age <= SAVED_MAX_S
        && saved["swept_at"] == json!(swept_at(common))
        && saved["stamp"] == json!(stamp(common, &paths))
}

/// G778 (owner, 2026-11-17: a panel that shows bay was still very long to load; a seat's
/// `bay status --deadline 0` took 8-11 s and a read of one change 4.5 s alone and 77 s beside
/// the panel's other reads, each built again from the store because no saved answer served a
/// linked worktree, a named change, or a store that had moved): the answer this machine last
/// saved for a read of `change` at `dir` as this actor, whatever moved since, with its age in
/// `detail.age_s` and `detail.from_store` true, and whether it is old enough to be read again in
/// the background ([`REFRESH_AFTER_S`]). `None` when none was saved.
#[must_use]
pub fn from_store(dir: &Path, change: Option<&str>) -> Option<(BayResult, bool)> {
    if off() {
        return None;
    }
    let (root, common) = checkout(dir)?;
    let (mut saved, age) = read(&common, &root, change)?;
    // G778b: an answer whose refs and views stand as they were is the answer now, while it is
    // younger than what the clock judges in it allows ([`SAVED_MAX_S`]): served as current, with
    // no read again.
    let current = age <= SAVED_MAX_S
        && !saved["quick"].is_null()
        && saved["swept_at"] == json!(swept_at(&common))
        && serde_json::from_value::<Vec<String>>(saved["views"].clone())
            .is_ok_and(|views| saved["quick"] == quick(&common, &views));
    let stands = current || age < refresh_after_s();
    let mut detail = saved["detail"].take();
    detail["from_store"] = json!(true);
    detail["current"] = json!(current);
    detail["age_s"] = json!(age);
    detail["saved_s"] = json!(age);
    if let Some((_, unix)) = crate::sweep::last(&common)
        && detail.get("stale_s").is_some_and(|stale| !stale.is_null())
    {
        detail["stale_s"] = json!(crate::live::unix_now().saturating_sub(unix));
    }
    let mut result = BayResult::ok(detail);
    if let Some(change) = saved["change"].as_str() {
        result = result.with_change(change.to_owned());
    }
    result.freshness = saved["freshness"].as_object().and_then(|fresh| {
        Some(Freshness {
            trunk: fresh.get("trunk")?.as_str()?.to_owned(),
            fetched_at: fresh.get("fetched_at")?.as_str()?.to_owned(),
        })
    });
    Some((result, !stands))
}

/// How old, in seconds, a saved answer served at once may be before it is read again in the
/// background for the next read: judged by age alone, since stamping the store costs seconds.
const REFRESH_AFTER_S: u64 = 5;

/// [`REFRESH_AFTER_S`], or in a test build `BAY_TEST_REFRESH_AFTER_S`.
fn refresh_after_s() -> u64 {
    std::env::var("BAY_TEST_REFRESH_AFTER_S")
        .ok()
        .filter(|_| crate::version::TEST_HOOKS)
        .and_then(|text| text.parse().ok())
        .unwrap_or(REFRESH_AFTER_S)
}

/// How long one background read again of a saved answer holds its lock before a later read may
/// take it over as dead.
const REFRESH_HELD_S: u64 = 120;

/// In the process that reads a saved answer again for the next read, `BAY_SAVED_REFRESH` is set:
/// it builds the answer from the store and saves it, serving none.
pub const REFRESH_ENV: &str = "BAY_SAVED_REFRESH";

/// Whether this process reads a saved answer again ([`REFRESH_ENV`]).
#[must_use]
pub fn refreshing() -> bool {
    std::env::var_os(REFRESH_ENV).is_some()
}

/// The lock of the background read again of the answer of `change` at `root`.
fn refresh_lock(common: &Path, root: &Path, change: Option<&str>) -> PathBuf {
    file(common, root, &actor(), change).with_extension("refresh")
}

/// G778: has the answer of a read of `change` at `dir` built again from the store and saved, by a
/// `bay status` of its own at low priority that outlives this one: one at a time for one answer,
/// under a lock the read again drops when it ends ([`refreshed`]). Best effort.
pub fn refresh(dir: &Path, change: Option<&str>) {
    let Some((root, common)) = checkout(dir) else {
        return;
    };
    let lock = refresh_lock(&common, &root, change);
    let held = std::fs::metadata(&lock)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.elapsed().ok())
        .is_some_and(|elapsed| elapsed.as_secs() < REFRESH_HELD_S);
    if held {
        return;
    }
    if let Some(parent) = lock.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&lock, std::process::id().to_string()).is_err() {
        return;
    }
    let Ok(bay) = std::env::current_exe() else {
        return;
    };
    // Below the reads people wait for, as the store's own compaction runs (G762).
    let mut cmd = if cfg!(unix) {
        let mut nice = std::process::Command::new("nice");
        nice.args(["-n", "10"]).arg(bay);
        nice
    } else {
        std::process::Command::new(bay)
    };
    cmd.arg("status");
    if let Some(change) = change {
        cmd.arg(change);
    }
    cmd.args(["--json", "--deadline", "0"])
        .current_dir(&root)
        .env(REFRESH_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::platform::detach(&mut cmd);
    match cmd.spawn() {
        Ok(child) => crate::platform::reap(child),
        Err(_) => {
            let _ = std::fs::remove_file(&lock);
        }
    }
}

/// The end of a background read again ([`refresh`]): its lock goes, so the next read that finds
/// the store moved may start another.
pub fn refreshed(dir: &Path, change: Option<&str>) {
    if !refreshing() {
        return;
    }
    if let Some((root, common)) = checkout(dir) {
        let _ = std::fs::remove_file(refresh_lock(&common, &root, change));
    }
}

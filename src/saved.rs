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

/// The saved answer of the store whose common directory is `common`.
fn file(common: &Path) -> PathBuf {
    common.join("bay").join("status-answer.json")
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
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(format!("{dir}/{}", entry.file_name().to_string_lossy()));
            }
        }
        paths.push(dir);
    }
    paths.sort();
    paths
}

/// The stamp of `paths` in the store at `common`.
fn stamp(common: &Path, paths: &[String]) -> Vec<Value> {
    paths.iter().map(|path| mark(&common.join(path))).collect()
}

/// The sweep record's time, which a saved answer was built after.
fn swept_at(common: &Path) -> Option<String> {
    crate::sweep::last(common).map(|(at, _)| at)
}

/// The actor a listing is read as.
fn actor(common: &Path) -> Option<String> {
    crate::change::current_actor(common)
        .ok()
        .map(|actor| actor.name)
}

/// What a listing's answer is built from, stamped before it is built: saved by [`Before::save`]
/// only when the store is as it was after the answer is built.
pub struct Before {
    root: PathBuf,
    common: PathBuf,
    paths: Vec<String>,
    stamp: Vec<Value>,
    swept_at: Option<String>,
}

impl Before {
    /// The store of `dir` before an answer of it is built; `None` for a view or a linked
    /// worktree.
    #[must_use]
    pub fn of(dir: &Path) -> Option<Self> {
        if off() {
            return None;
        }
        let (root, common) = own_checkout(dir)?;
        let paths = paths(&common);
        let stamp = stamp(&common, &paths);
        Some(Self {
            root,
            swept_at: swept_at(&common),
            common,
            paths,
            stamp,
        })
    }

    /// Saves `result`, a listing of the store, when nothing it was built from moved while it was
    /// built. A talk accept is never saved: it happened once.
    pub fn save(self, result: &BayResult) {
        if result.decision != crate::result::Decision::Ok
            || stamp(&self.common, &self.paths) != self.stamp
            || swept_at(&self.common) != self.swept_at
        {
            let _ = std::fs::remove_file(file(&self.common));
            return;
        }
        let Some(mut detail) = result.detail.clone() else {
            return;
        };
        if let Some(detail) = detail.as_object_mut() {
            detail.remove("talk_accept");
        }
        let saved = json!({
            "bay": crate::version::BAY,
            "root": self.root,
            "actor": actor(&self.common),
            "swept_at": self.swept_at,
            "saved": crate::live::unix_now(),
            "paths": self.paths,
            "stamp": self.stamp,
            "change": result.change,
            "freshness": result.freshness.as_ref().map(|fresh| json!({
                "trunk": fresh.trunk,
                "fetched_at": fresh.fetched_at,
            })),
            "detail": detail,
        });
        replace(&file(&self.common), &saved.to_string());
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
    let text = std::fs::read_to_string(file(&common)).ok()?;
    let saved: Value = serde_json::from_str(&text).ok()?;
    let now = crate::live::unix_now();
    let age = now.checked_sub(saved["saved"].as_u64()?)?;
    let paths: Vec<String> = serde_json::from_value(saved["paths"].clone()).ok()?;
    let fresh = saved["bay"] == crate::version::BAY
        && age <= SAVED_MAX_S
        && saved["root"] == json!(root)
        && saved["swept_at"] == json!(swept_at(&common))
        && saved["actor"] == json!(actor(&common))
        && saved["stamp"] == json!(stamp(&common, &paths));
    if !fresh {
        return None;
    }
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

//! G665: `bay doctor`'s storage row: where a clone's disk went, read only.
//!
//! Git's objects, half-written packs, this repository's views and check scratch, the worktrees bay
//! did not make, and what the next `bay start` would free ([`crate::start::would_release`], the
//! decision that start itself takes). Nothing here removes anything. Sizing a large tree takes
//! seconds, so all of it shares one time budget, and a row cut short says so in `partial`.

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::git::Git;
use crate::repo::Repo;

/// How long the row may read the disk.
const BUDGET: Duration = Duration::from_secs(3);
/// The largest views the row names.
const LARGEST: usize = 4;
/// The other worktrees the row names.
const OTHERS: usize = 50;

use crate::version::TEST_HOOKS;

fn budget() -> Duration {
    std::env::var("BAY_TEST_STORAGE_BUDGET_MS")
        .ok()
        .filter(|_| TEST_HOOKS)
        .and_then(|ms| ms.parse().ok())
        .map_or(BUDGET, Duration::from_millis)
}

/// The bytes under `dir`, links not followed, and whether it read them all by `deadline`.
fn size_of(dir: &Path, deadline: Instant) -> (u64, bool) {
    let mut bytes = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            if Instant::now() >= deadline {
                return (bytes, false);
            }
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                bytes += meta.len();
            }
        }
    }
    (bytes, true)
}

/// The bytes `git count-objects` reports for the store.
fn objects_bytes(git: &Git) -> Option<u64> {
    let out = git.run(&["count-objects", "-v"]).ok()?;
    let kib: u64 = out
        .lines()
        .filter_map(|line| line.split_once(": "))
        .filter(|(key, _)| matches!(*key, "size" | "size-pack" | "size-garbage"))
        .filter_map(|(_, value)| value.trim().parse::<u64>().ok())
        .sum();
    Some(kib * 1024)
}

/// Each half-written pack's size, and whether a start would remove it
/// ([`crate::executor::tmp_pack_is_stale`]).
fn half_written_packs(common_dir: &Path) -> (u64, u64, u64) {
    let Ok(entries) = std::fs::read_dir(common_dir.join("objects").join("pack")) else {
        return (0, 0, 0);
    };
    let (mut count, mut bytes, mut stale_bytes) = (0, 0, 0);
    for entry in entries.flatten() {
        if !crate::executor::is_tmp_pack(&entry) {
            continue;
        }
        let size = entry.metadata().map_or(0, |meta| meta.len());
        count += 1;
        bytes += size;
        if crate::executor::tmp_pack_is_stale(&entry) {
            stale_bytes += size;
        }
    }
    (count, bytes, stale_bytes)
}

/// The worktrees of this repository bay did not make: neither its main work tree nor a view.
fn other_worktrees(repo: &Repo) -> Vec<String> {
    let real = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let views: Vec<_> = repo.views_dirs().iter().map(|dir| real(dir)).collect();
    let main = real(&repo.work_root);
    let Ok(listed) = repo.git.run(&["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    listed
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter(|path| {
            let path = real(Path::new(path));
            path != main
                && !path
                    .parent()
                    .is_some_and(|parent| views.iter().any(|dir| dir == parent))
        })
        .map(str::to_owned)
        .collect()
}

/// The storage row of the repository at `dir`. Advisory: it never fails `bay doctor`.
#[must_use]
pub fn row(git: &Git, dir: &Path) -> Value {
    let mut row = json!({
        "name": "storage",
        "state": "green",
        "objects_bytes": objects_bytes(git),
    });
    let Ok(repo) = Repo::open_known(dir) else {
        return row;
    };
    let deadline = Instant::now() + budget();
    let in_time = || Instant::now() < deadline;
    let (packs, packs_bytes, stale_bytes) = half_written_packs(&repo.common_dir);

    // What a start would free comes first: it is the row's answer, and the cheapest to bound.
    let (free, mut whole) = crate::start::would_release(&repo, dir, &in_time);

    let ids = crate::view::all(&repo);
    let mut sized: Vec<(String, u64)> = Vec::new();
    for id in &ids {
        let (bytes, complete) = size_of(&repo.view_path(id), deadline);
        whole &= complete;
        if !complete {
            break;
        }
        sized.push((id.clone(), bytes));
    }
    let views_bytes: u64 = sized.iter().map(|(_, bytes)| bytes).sum();
    sized.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
    let largest: Vec<Value> = sized
        .iter()
        .take(LARGEST)
        .map(|(id, bytes)| json!({"change": id, "bytes": bytes}))
        .collect();

    let scratch = crate::check::scratch_root(&repo.common_dir).ok();
    let (scratch_bytes, complete) = scratch.map_or((0, true), |root| size_of(&root, deadline));
    whole &= complete;

    let others = other_worktrees(&repo);
    row["half_written_packs"] = json!({"count": packs, "bytes": packs_bytes});
    row["views"] = json!({
        "count": ids.len(),
        "measured": sized.len(),
        "bytes": views_bytes,
        "largest": largest,
    });
    row["check_scratch_bytes"] = json!(scratch_bytes);
    row["other_worktrees"] = json!({
        "count": others.len(),
        "paths": others.iter().take(OTHERS).collect::<Vec<_>>(),
    });
    row["next_start_frees"] = json!({"views": free, "half_written_pack_bytes": stale_bytes});
    row["partial"] = json!(!whole);
    row
}

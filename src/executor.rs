//! `bay executor run`: the supervised local executor (`DESIGN` § 9a,
//! `INTERFACES` § 3a).
//!
//! It fetches the change chains from the remote and drives every queued operation to a stated
//! outcome: published, or blocked with a precise repair. The agent that submitted need not be
//! there. With `--once` it makes one pass and exits (for a CI schedule or a test); otherwise
//! it keeps polling.
//!
//! The contract's actions map onto bay: `submit` is the request in the change chain (and, with
//! `[executor] command`, a call to that adapter); `inspect` is `bay status`; `resume` is running
//! the operation again from the chain, which never repeats a persisted candidate; `cancel` is a
//! newer revision (kickstart item 19b); `complete` is the evidence notes. The ownership claim
//! with epochs is item 16; leases and takeover are item 19a.
//!
//! While it owns an operation the executor holds a box-local lock: a file named by its pid in
//! `executor.lock.d/` in bay's state directory, one per executor, which only its owner removes,
//! so two executors on one box never free each other's hold. `bay update` waits while any of
//! them names a live pid (`UPDATE_IN_USE`). Between
//! operations it exits cleanly when its binary changed since it started, so its supervisor
//! restarts it on the new one (owner ruling 2026-10-18).

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::change::{CHANGES_PREFIX, Chain, ChangeRecord, OpState};
use crate::publish;
use crate::queue::Queue;
use crate::receipts;
use crate::repo::Repo;
use crate::result::BayResult;
use crate::signing::Keys;

#[must_use]
pub fn run(dir: &Path, once: bool, interval: Duration) -> BayResult {
    let repo = match Repo::open(dir) {
        Ok(repo) => repo,
        Err(message) => return BayResult::failed(message),
    };
    // The supervisor's own sightings of queue heads, on its monotonic clock (§ 6).
    let mut heads: HashMap<u64, Instant> = HashMap::new();
    let binary = Binary::now();
    loop {
        // A release that could not be pushed never stops the supervisor, which every queued
        // operation depends on: it is reported and tried again on the next pass (§ 6, § 9a).
        let unreleased = deadlines(&repo, &mut heads).err();
        let ran = match pass(&repo, &binary) {
            Ok(ran) => ran,
            Err(message) => return BayResult::failed(message),
        };
        if binary.changed() {
            return BayResult::ok(json!({"ran": ran, "restart": "the binary changed"}));
        }
        if once {
            let mut detail = json!({"ran": ran});
            if let Some(message) = &unreleased {
                detail["unreleased"] = json!(message);
            }
            return BayResult::ok(detail);
        }
        if let Some(message) = &unreleased {
            eprintln!("bay executor: {message}");
        }
        // A looping supervisor never reaches the end of `main`: a pass that fetched trunk
        // leaves the coordination cache now (`crate::coord`).
        crate::coord::after_command();
        std::thread::sleep(interval);
    }
}

/// The running binary as it was at start: its path and what identifies the file there.
struct Binary {
    path: Option<PathBuf>,
    stamp: Option<(u64, Option<std::time::SystemTime>, u64)>,
}

impl Binary {
    fn now() -> Self {
        let path = std::env::current_exe().ok();
        let stamp = path.as_deref().and_then(stamp);
        Self { path, stamp }
    }

    /// The file at the path is another one now (inode, or modification time and size).
    fn changed(&self) -> bool {
        self.path
            .as_deref()
            .is_some_and(|path| stamp(path) != self.stamp)
    }
}

fn stamp(path: &Path) -> Option<(u64, Option<std::time::SystemTime>, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    Some((inode, meta.modified().ok(), meta.len()))
}

/// The box-local lock an executor holds while it owns an operation: `executor.lock.d/<pid>`,
/// written by a temporary file and a rename, removed on drop by its owner only.
struct Lock(PathBuf);

impl Lock {
    fn hold() -> Option<Self> {
        let dir = crate::update::state_dir()?.join(LOCKS);
        std::fs::create_dir_all(&dir).ok()?;
        let pid = std::process::id();
        let tmp = dir.join(format!(".{pid}.tmp"));
        std::fs::write(&tmp, format!("{pid}\n")).ok()?;
        let path = dir.join(pid.to_string());
        std::fs::rename(&tmp, &path).ok()?;
        Some(Self(path))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const LOCKS: &str = "executor.lock.d";

/// The pid of a live executor on this box that holds an operation. A file whose pid is gone is
/// stale and ignored; a pid this process may not signal exists, so it counts as live.
#[must_use]
pub fn busy() -> Option<u32> {
    let dir = crate::update::state_dir()?.join(LOCKS);
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .find(|&pid| alive(pid))
}

/// `kill -0`: success, or a refusal to signal (EPERM), means the process exists.
fn alive(pid: u32) -> bool {
    let mut probe = Command::new("kill");
    probe.args(["-0", &pid.to_string()]).env("LC_ALL", "C");
    crate::git::run_bounded(probe, "kill -0").is_ok_and(|output| {
        output.status.success()
            || String::from_utf8_lossy(&output.stderr)
                .to_ascii_lowercase()
                .contains("not permitted")
    })
}

/// Releases a queue head that made no progress within its deadline, measured from this
/// supervisor's first sighting of it as head; a late observer or a restart only delays it.
/// An error is a release the remote refused or did not take; the next pass tries it again.
fn deadlines(repo: &Repo, heads: &mut HashMap<u64, Instant>) -> Result<(), String> {
    let mut queue = Queue::read(repo)?;
    // Heads whose operation already reached an outcome release at once, one after another.
    while let Some(head) = queue.head()
        && let Some(reason) = finished_head(repo, head)
    {
        let signer = Keys::load(&repo.git).executor;
        crate::queue::release(repo, signer.as_ref(), head.seq, &head.op_id, &reason)?;
        queue = Queue::read(repo)?;
    }
    heads.retain(|seq, _| queue.head().is_some_and(|head| head.seq == *seq));
    let Some(head) = queue.head() else {
        return Ok(());
    };
    let first = *heads.entry(head.seq).or_insert_with(Instant::now);
    if first.elapsed() >= repo.config.progress_deadline() {
        let signer = Keys::load(&repo.git).executor;
        crate::queue::release(repo, signer.as_ref(), head.seq, &head.op_id, "deadline")?;
        heads.remove(&head.seq);
    }
    Ok(())
}

/// The head always releases (§ 6): a head whose operation already reached an outcome
/// (published, blocked, failed or superseded) is never run again, so it would hold the queue
/// for good. Such a head is a second reservation of an operation that two executors queued at
/// once, or one whose release a crash lost. A change that the retention cut closed keeps no
/// operations, and an expired op id is never run again (§ 5, § 9a), so its place is finished
/// too and is released with the change's state. Returns the release reason, the operation's
/// outcome, or `None` while the operation can still run (or its chain cannot be read).
fn finished_head(repo: &Repo, head: &crate::queue::Entry) -> Option<String> {
    let name = crate::change::change_ref(&head.change);
    repo.git
        .run(&[
            "fetch",
            "-q",
            "--no-tags",
            repo.remote(),
            &format!("+{name}:{name}"),
        ])
        .ok()?;
    let (_, record) = Chain::new(&repo.git).read(&head.change).ok()?;
    let Some(op) = record.operations.iter().find(|op| op.op_id == head.op_id) else {
        return if record.retention_cut {
            word(record.state)
        } else {
            None
        };
    };
    if !op.state.is_terminal() && op.state != OpState::Blocked {
        return None;
    }
    Some(op.outcome.as_ref().map_or_else(
        || word(op.state).unwrap_or_default(),
        |outcome| outcome.to_lowercase(),
    ))
}

/// A state's snake-case word, as the change chain spells it.
fn word(state: impl serde::Serialize) -> Option<String> {
    serde_json::to_value(state)
        .ok()
        .and_then(|state| state.as_str().map(str::to_owned))
}

/// One pass: every operation that is neither terminal nor blocked runs to an outcome, each
/// under the box-local lock. It stops between operations once the binary changed.
fn pass(repo: &Repo, binary: &Binary) -> Result<Vec<Value>, String> {
    let _ = receipts::sync(repo);
    let refspec = format!("+{CHANGES_PREFIX}*:{CHANGES_PREFIX}*");
    repo.git
        .run(&["fetch", "-q", "--no-tags", repo.remote(), &refspec])
        .map_err(|error| error.message())?;
    let chain = Chain::new(&repo.git);
    let names = repo
        .git
        .run(&["for-each-ref", "--format=%(refname)", CHANGES_PREFIX])
        .map_err(|error| error.message())?;
    // Queued changes go first, in queue order, so one pass carries the queue as far as it can:
    // when the head lands, the next entry is head by the time it is reached.
    let queue = Queue::read(repo)?;
    let place = |id: &str| queue.of_change(id).map_or(u64::MAX, |entry| entry.seq);
    let mut ids: Vec<&str> = names
        .lines()
        .map(|name| name.trim_start_matches(CHANGES_PREFIX))
        .collect();
    ids.sort_by_key(|id| place(id));
    let records: Vec<ChangeRecord> = ids
        .iter()
        .filter_map(|id| chain.read(id).ok().map(|(_, record)| record))
        .collect();
    let mut ran = Vec::new();
    // An event a past run lost between the chain and the log is written now (G9), and a log
    // record whose push failed is pushed again.
    let mut recovered = match publish::recover_events(repo, &records) {
        Ok(0) => None,
        Ok(written) => Some(json!({"events_recovered": written})),
        Err(error) => Some(json!({"events_recovered": null, "error": error})),
    };
    crate::log::push(repo, &Keys::load(&repo.git).executor_name);
    for record in &records {
        let id = record.id.as_str();
        let queued: Vec<String> = record
            .operations
            .iter()
            .filter(|op| !op.state.is_terminal() && op.state != OpState::Blocked)
            .map(|op| op.op_id.clone())
            .collect();
        if queued.is_empty() {
            if let Ok(Some(head)) = publish::retain(repo, id) {
                ran.push(json!({"change": id, "retention_cut": head}));
            }
            continue;
        }
        for op_id in queued {
            if binary.changed() {
                ran.extend(recovered.take());
                return Ok(ran);
            }
            let step = Cell::new("read");
            let lock = Lock::hold();
            let result =
                publish::execute(repo, id, &op_id, &step).unwrap_or_else(BayResult::failed);
            drop(lock);
            let result = publish::with_receipt(repo, id, step.get(), result).with_op(op_id.clone());
            ran.push(json!({
                "change": id,
                "op_id": op_id,
                "decision": result.decision,
                "reason": result.reason,
                "detail": result.detail,
            }));
        }
    }
    ran.extend(recovered.take());
    Ok(ran)
}

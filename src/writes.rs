//! The files a step wrote, from the view's own Git status (`DESIGN` § 11a).
//!
//! At each post-tool and stop event the hook reads the view's changed paths (`git status
//! --porcelain=v2`) and their working-tree blob ids (`git hash-object`), and diffs them against
//! the ids it kept at that agent's previous event. Status alone is cumulative for the view, so a
//! file edited in an earlier step and again now would otherwise look unchanged.
//!
//! The kept ids are the hook's own state, one small file per agent of a session
//! (`<git-common-dir>/bay/session-state/<session>/<agent>.json`), not record fields: a record
//! line does not grow with the view, and no hook reads the whole record. A subagent shares its
//! parent's session id but keeps its own ids, so a parent's step never takes a background
//! subagent's write away from it (both list a write made while both ran: status cannot say who
//! wrote it).
//!
//! The hook is on the synchronous path before the next model call, so its cost is bounded: two
//! git calls, each stopped at [`CALL_TIMEOUT`]; at most [`MAX_PATHS`] changed paths; a file whose
//! size, times and inode did not move since it was hashed is not hashed again; at most
//! [`MAX_HASH_BYTES`] hashed per event. Past a bound, or when the status cannot be read, the
//! step's writes are unknown (`wrote_unknown`), never a guess, and the kept ids stay as they were,
//! so the next step that can be read lists them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// The most changed paths one event reads.
pub const MAX_PATHS: usize = 2048;
/// The most bytes one event hashes.
pub const MAX_HASH_BYTES: u64 = 64 * 1024 * 1024;
/// Each git call of a hook stops here.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// A file modified this close to the read is hashed again at the next event: a later write in
/// the same timestamp tick would leave its stat unchanged (Git's racy-clean case).
const RACY: Duration = Duration::from_secs(2);
/// A kept-ids file larger than this is not this hook's (at most [`MAX_PATHS`] entries).
const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;

/// One changed path: its working-tree blob id (`deleted` when the path is gone,
/// `symlink:<target>` for a link) and the stat it was hashed at, empty when not to be trusted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    stat: String,
}

type Ids = BTreeMap<String, Entry>;

/// What an event does with the view's status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The agent's start: keep the view's ids as its baseline, unless it already has one (a
    /// compacted or resumed session keeps its baseline).
    Baseline,
    /// A post-tool or stop event: list what moved since the agent's last ids, keep the new ones.
    Diff,
    /// A subagent's stop: [`Step::Diff`], then drop its ids; the subagent is done.
    DiffAndForget,
}

/// What a diffing event records.
#[derive(Debug, PartialEq, Eq)]
pub enum Wrote {
    /// The paths whose blob id moved, sorted.
    Paths(Vec<String>),
    /// The writes could not be read; the reason.
    Unknown(&'static str),
}

/// The kept-ids file of one agent of one session: `session` for the session itself, else
/// `agent-<agent_id>`. `None` when the agent id is not a plain file name.
#[must_use]
pub fn state_path(state_dir: &Path, agent: Option<&str>) -> Option<PathBuf> {
    match agent {
        None => Some(state_dir.join("session.json")),
        Some(agent) => crate::session::SessionId::parse(agent)
            .map(|agent| state_dir.join(format!("agent-{}.json", agent.as_str()))),
    }
}

/// Runs `step` for the view at `view` with the agent's kept ids at `state`. `None` for a
/// baseline, which records nothing.
#[must_use]
pub fn step(view: &Path, state: &Path, step: Step) -> Option<Wrote> {
    if step == Step::Baseline {
        if !state.exists()
            && let Ok(now) = read_view(view, &Ids::new())
        {
            store(state, &now);
        }
        return None;
    }
    // No kept ids (the agent's start was not recorded): every changed path counts as its write.
    let before = load(state);
    let now = match read_view(view, &before) {
        Ok(now) => now,
        Err(reason) => return Some(Wrote::Unknown(reason)),
    };
    let wrote = written(&before, &now);
    if step == Step::DiffAndForget {
        let _ = std::fs::remove_file(state);
    } else {
        store(state, &now);
    }
    Some(Wrote::Paths(wrote))
}

fn load(state: &Path) -> Ids {
    let small = std::fs::metadata(state).is_ok_and(|meta| meta.len() <= MAX_STATE_BYTES);
    if !small {
        return Ids::new();
    }
    std::fs::read(state)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Replaces the kept ids whole (write, then rename), so a reader never sees half a file.
fn store(state: &Path, ids: &Ids) {
    let Some(dir) = state.parent() else {
        return;
    };
    let Ok(bytes) = serde_json::to_vec(ids) else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = state.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, state).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The view's changed paths and their ids, reusing an id from `before` whose stat matches.
fn read_view(view: &Path, before: &Ids) -> Result<Ids, &'static str> {
    let started = SystemTime::now();
    let status = git(
        view,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
        None,
    )
    .ok_or("status_unreadable")?;
    let paths = changed_paths(&status);
    if paths.len() > MAX_PATHS {
        return Err("too_many_paths");
    }
    let mut now = Ids::new();
    let mut to_hash = Vec::new();
    let mut bytes = 0u64;
    for raw in paths {
        let key = String::from_utf8_lossy(raw).into_owned();
        let full = view.join(path_of(raw)?);
        let meta = match std::fs::symlink_metadata(&full) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                now.insert(
                    key,
                    Entry {
                        id: "deleted".to_owned(),
                        stat: String::new(),
                    },
                );
                continue;
            }
            Err(_) => return Err("path_unreadable"),
        };
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&full).map_err(|_| "path_unreadable")?;
            now.insert(
                key,
                Entry {
                    id: format!("symlink:{}", target.to_string_lossy()),
                    stat: String::new(),
                },
            );
            continue;
        }
        // A directory (an untracked nested repository, a submodule), a fifo or a socket holds
        // no blob of this view.
        if !meta.is_file() {
            continue;
        }
        let stat = stat_of(&meta, started);
        if let Some(old) = before.get(&key)
            && !stat.is_empty()
            && old.stat == stat
        {
            now.insert(key, old.clone());
            continue;
        }
        if raw.contains(&b'\n') {
            return Err("path_has_newline");
        }
        bytes += meta.len();
        to_hash.push((key, raw, stat));
    }
    if bytes > MAX_HASH_BYTES {
        return Err("too_many_bytes");
    }
    if to_hash.is_empty() {
        return Ok(now);
    }
    let mut input = Vec::new();
    for (_, raw, _) in &to_hash {
        input.extend_from_slice(raw);
        input.push(b'\n');
    }
    // `--no-filters`: the raw working-tree bytes, so no clean filter or filter process runs
    // inside a hook.
    let out = git(
        view,
        &["hash-object", "--no-filters", "--stdin-paths"],
        Some(&input),
    )
    .ok_or("hash_failed")?;
    let text = String::from_utf8(out).map_err(|_| "hash_failed")?;
    let ids: Vec<&str> = text.lines().collect();
    if ids.len() != to_hash.len() {
        return Err("hash_failed");
    }
    for ((key, _, stat), id) in to_hash.into_iter().zip(ids) {
        now.insert(
            key,
            Entry {
                id: id.to_owned(),
                stat,
            },
        );
    }
    Ok(now)
}

/// The paths of `git status --porcelain=v2 -z` entries: `1 XY sub mH mI mW hH hI path`,
/// `u XY sub m1 m2 m3 mW h1 h2 h3 path`, `? path`, and `2 … path` followed by its original path
/// (not printed under `--no-renames`, skipped all the same).
fn changed_paths(status: &[u8]) -> Vec<&[u8]> {
    let mut paths = Vec::new();
    let mut entries = status.split(|byte| *byte == 0);
    while let Some(entry) = entries.next() {
        let field = |count: usize| entry.splitn(count, |byte| *byte == b' ').nth(count - 1);
        let path = match entry.first() {
            Some(b'1') => field(9),
            Some(b'u') => field(11),
            Some(b'?') => entry.get(2..),
            Some(b'2') => {
                let _ = entries.next();
                field(10)
            }
            _ => None,
        };
        if let Some(path) = path.filter(|path| !path.is_empty()) {
            paths.push(path);
        }
    }
    paths
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps, reason = "the non-Unix path_of can fail")]
fn path_of(raw: &[u8]) -> Result<&Path, &'static str> {
    use std::os::unix::ffi::OsStrExt;
    Ok(Path::new(std::ffi::OsStr::from_bytes(raw)))
}

#[cfg(not(unix))]
fn path_of(raw: &[u8]) -> Result<&Path, &'static str> {
    std::str::from_utf8(raw)
        .map(Path::new)
        .map_err(|_| "path_not_utf8")
}

/// The stat a kept id is trusted at: size, modification and change times, inode. Empty for a
/// file modified within [`RACY`] of the read.
fn stat_of(meta: &std::fs::Metadata, started: SystemTime) -> String {
    let Ok(modified) = meta.modified() else {
        return String::new();
    };
    let settled = started
        .duration_since(modified)
        .is_ok_and(|age| age >= RACY);
    if !settled {
        return String::new();
    }
    let mtime = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|time| time.as_nanos())
        .unwrap_or_default();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{}:{mtime}:{}.{}:{}:{}",
            meta.len(),
            meta.ctime(),
            meta.ctime_nsec(),
            meta.dev(),
            meta.ino()
        )
    }
    #[cfg(not(unix))]
    {
        format!("{}:{mtime}", meta.len())
    }
}

/// `git -C <view> <args>` with optional stdin, stopped at [`CALL_TIMEOUT`]; its stdout on
/// success. A hook never takes the view's index lock from under the agent's own git.
fn git(view: &Path, args: &[&str], input: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(view)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    let output = crate::git::run_bounded_for(cmd, &args.join(" "), input, CALL_TIMEOUT).ok()?;
    output.status.success().then_some(output.stdout)
}

/// The paths whose id differs between two reads: written, deleted, or put back to the trunk's
/// content. Sorted, since both reads are.
fn written(before: &Ids, after: &Ids) -> Vec<String> {
    let mut paths: Vec<String> = after
        .iter()
        .filter(|(path, entry)| before.get(*path).map(|old| &old.id) != Some(&entry.id))
        .map(|(path, _)| path.clone())
        .chain(
            before
                .keys()
                .filter(|path| !after.contains_key(*path))
                .cloned(),
        )
        .collect();
    paths.sort();
    paths
}

#[cfg(test)]
mod tests {
    use super::changed_paths;

    #[test]
    fn every_status_entry_kind_names_its_path() {
        let status = b"1 .M N... 100644 100644 100644 aa bb src/a b.rs\0\
            1 .M S.M. 160000 160000 160000 aa bb vendor/sub\0\
            u UU N... 100644 100644 100644 100644 aa bb cc src/conflict.rs\0\
            2 R. N... 100644 100644 100644 aa bb R100 src/new name.rs\0src/old.rs\0\
            ? src/nested/\0\
            ? notes.txt\0";
        let paths: Vec<String> = changed_paths(status)
            .into_iter()
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect();
        assert_eq!(
            paths,
            [
                "src/a b.rs",
                "vendor/sub",
                "src/conflict.rs",
                "src/new name.rs",
                "src/nested/",
                "notes.txt"
            ]
        );
    }
}

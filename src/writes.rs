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
//! so the next step that can be read lists them. The session's stop adds two bounded calls
//! ([`stop`], each stopped at [`CALL_TIMEOUT`]): whether the view holds edits no draft records.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
            && let Ok(now) = read_view(view, &Ids::new()).map(|read| read.ids)
        {
            store(state, &now);
        }
        return None;
    }
    Some(match diff(view, state, step) {
        Ok((wrote, _)) => Wrote::Paths(wrote),
        Err(reason) => Wrote::Unknown(reason),
    })
}

/// What the view holds against its change's latest draft at the session's stop.
#[derive(Debug, PartialEq, Eq)]
pub enum Edits {
    /// Every edit in the view is in the latest draft.
    Checkpointed,
    /// The view holds edits that no checkpoint records; a digest of the draft and of the view's
    /// read, the same for the same edits.
    Uncheckpointed(String),
    /// The edits could not be read; the reason.
    Unknown(&'static str),
}

/// The session's stop (ruling R7): [`Step::Diff`], and whether the view holds edits that its
/// change's latest draft does not record (uncheckpointed edits, `DESIGN` § 4). `head` is the
/// change's state commit and `cut` whether it is a retention cut.
///
/// A checkpoint stages every file through the view's own index and writes that index as the
/// draft (`crate::change::freeze_draft`), so the draft records the view's edits when the index
/// holds the draft's entries and the working tree matches the index. The status the step
/// already read says both, as Git compares them (a link by its target, a file as its clean
/// filter stores it), and two more bounded calls give the draft and the paths it changed from
/// the view's `HEAD` (`git diff-tree`, objects only). A path no status lists holds `HEAD`'s
/// entry in the index and the working tree. A sparse view's absent files are not listed, so
/// they are not edits; an untracked file, a nested repository or a conflict always is.
#[must_use]
pub fn stop(view: &Path, state: &Path, head: &str, cut: bool) -> (Wrote, Edits) {
    match diff(view, state, Step::Diff) {
        Ok((wrote, now)) => {
            let edits = match uncheckpointed(view, head, cut, &now) {
                Ok(None) => Edits::Checkpointed,
                Ok(Some(digest)) => Edits::Uncheckpointed(digest),
                Err(reason) => Edits::Unknown(reason),
            };
            (Wrote::Paths(wrote), edits)
        }
        Err(reason) => (Wrote::Unknown(reason), Edits::Unknown(reason)),
    }
}

/// A diffing step: the paths written since the agent's kept ids, and the view's read.
fn diff(view: &Path, state: &Path, step: Step) -> Result<(Vec<String>, Read), &'static str> {
    // No kept ids (the agent's start was not recorded): every changed path counts as its write.
    let before = load(state);
    let now = read_view(view, &before)?;
    let wrote = written(&before, &now.ids);
    if step == Step::DiffAndForget {
        let _ = std::fs::remove_file(state);
    } else {
        store(state, &now.ids);
    }
    Ok((wrote, now))
}

/// A tree entry: its mode and object id.
type TreeEntry = (String, String);

/// One status entry, as far as a checkpoint is concerned.
#[derive(Debug, PartialEq, Eq)]
enum Listed {
    /// `?`: in no index, so in no checkpoint.
    Untracked,
    /// `u`: the view is mid-conflict.
    Unmerged,
    /// `1` or `2`: `HEAD`'s and the index's entries (`None` where absent), and whether the
    /// working tree matches the index.
    Staged {
        head: Option<TreeEntry>,
        index: Option<TreeEntry>,
        clean: bool,
    },
}

/// One read of the view: the listed paths' working-tree ids, and each status entry.
#[derive(Debug)]
struct Read {
    ids: Ids,
    listed: Vec<(String, Listed)>,
}

/// `None` when the latest draft records every edit in the view, else a digest of the draft
/// and the read.
fn uncheckpointed(
    view: &Path,
    head: &str,
    cut: bool,
    now: &Read,
) -> Result<Option<String>, &'static str> {
    let draft = latest_draft(view, head, cut)?;
    let out = git(
        view,
        &["diff-tree", "-r", "-z", "--no-renames", "HEAD", &draft],
        None,
    )
    .ok_or("draft_unreadable")?;
    let drafted = draft_entries(&out)?;
    if !edits_in(&now.listed, &drafted) {
        return Ok(None);
    }
    // The ids, not the stats they were hashed at: a file that only settles past the racy window,
    // or is touched, holds the same edits.
    let ids: Vec<(&String, &String)> = now
        .ids
        .iter()
        .map(|(path, entry)| (path, &entry.id))
        .collect();
    let read = format!("{draft}\0{:?}\0{ids:?}", now.listed);
    Ok(Some(crate::agent_settings::hex(&Sha256::digest(
        read.as_bytes(),
    ))))
}

/// The latest draft of the chain whose head is the state commit `head`, as
/// [`crate::change::Chain::drafts`] reads it, from one call bounded like the hook's others.
fn latest_draft(view: &Path, head: &str, cut: bool) -> Result<String, &'static str> {
    let out =
        git(view, &["rev-list", "--parents", "-n", "1", head], None).ok_or("draft_unreadable")?;
    let text = String::from_utf8(out).map_err(|_| "draft_unreadable")?;
    let parents: Vec<&str> = text.split_whitespace().skip(1).collect();
    match parents.as_slice() {
        // A cut head is a root: its first parent is the landed revision.
        [revision, ..] if cut => Ok((*revision).to_owned()),
        [draft] | [_, draft] | [_, draft, _] => Ok((*draft).to_owned()),
        _ => Err("draft_unreadable"),
    }
}

/// Whether the status entries `listed` hold an edit the draft, which changed `drafted` from
/// `HEAD`, does not record.
fn edits_in(listed: &[(String, Listed)], drafted: &Drafted) -> bool {
    // A path the draft changed that the status does not list holds `HEAD`'s entry.
    let names: std::collections::BTreeSet<&str> =
        listed.iter().map(|(path, _)| path.as_str()).collect();
    if drafted.keys().any(|path| !names.contains(path.as_str())) {
        return true;
    }
    listed.iter().any(|(path, entry)| match entry {
        Listed::Untracked | Listed::Unmerged => true,
        Listed::Staged { head, index, clean } => {
            // What the draft holds here: its own entry where it changed the path, else `HEAD`'s.
            let want = drafted.get(path).unwrap_or(head);
            !clean || index != want
        }
    })
}

/// The entries of `git diff-tree -r -z` raw output (`:mode mode id id status\0path\0`): each
/// path with the draft's mode and id, `None` where the draft deleted it.
type Drafted = BTreeMap<String, Option<(String, String)>>;

fn draft_entries(out: &[u8]) -> Result<Drafted, &'static str> {
    let mut drafted = Drafted::new();
    let mut fields = out.split(|byte| *byte == 0);
    while let Some(head) = fields.next() {
        if head.is_empty() {
            continue;
        }
        let head = std::str::from_utf8(head).map_err(|_| "draft_unreadable")?;
        let path = fields.next().ok_or("draft_unreadable")?;
        let parts: Vec<&str> = head.trim_start_matches(':').split(' ').collect();
        let [_, mode, _, id, _] = parts.as_slice() else {
            return Err("draft_unreadable");
        };
        if drafted.len() == MAX_PATHS {
            return Err("too_many_paths");
        }
        let entry =
            (!id.bytes().all(|byte| byte == b'0')).then(|| ((*mode).to_owned(), (*id).to_owned()));
        drafted.insert(String::from_utf8_lossy(path).into_owned(), entry);
    }
    Ok(drafted)
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
fn read_view(view: &Path, before: &Ids) -> Result<Read, &'static str> {
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
    let mut listed = Vec::new();
    for (raw, base) in paths {
        let key = String::from_utf8_lossy(raw).into_owned();
        listed.push((key.clone(), base));
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
        return Ok(Read { ids: now, listed });
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
    Ok(Read { ids: now, listed })
}

/// The paths of `git status --porcelain=v2 -z` entries: `1 XY sub mH mI mW hH hI path`,
/// `u XY sub m1 m2 m3 mW h1 h2 h3 path`, `? path`, and `2 … path` followed by its original path
/// (not printed under `--no-renames`, skipped all the same). Each with its entry.
fn changed_paths(status: &[u8]) -> Vec<(&[u8], Listed)> {
    let mut paths = Vec::new();
    let mut entries = status.split(|byte| *byte == 0);
    while let Some(entry) = entries.next() {
        let field = |count: usize| entry.splitn(count, |byte| *byte == b' ').nth(count - 1);
        let path = match entry.first() {
            Some(b'1') => field(9).map(|path| (path, staged(entry))),
            Some(b'u') => field(11).map(|path| (path, Listed::Unmerged)),
            Some(b'?') => entry.get(2..).map(|path| (path, Listed::Untracked)),
            Some(b'2') => {
                let _ = entries.next();
                field(10).map(|path| (path, staged(entry)))
            }
            _ => None,
        };
        if let Some(path) = path.filter(|(path, _)| !path.is_empty()) {
            paths.push(path);
        }
    }
    paths
}

/// A `1` or `2` entry's `HEAD` and index sides: `XY`, `mH`, `mI`, `hH` and `hI` are its
/// fields 1, 3, 4, 6 and 7. A zero mode is an absent side; `Y` is `.` when the working tree
/// matches the index.
fn staged(entry: &[u8]) -> Listed {
    let fields: Vec<String> = entry
        .split(|byte| *byte == b' ')
        .take(8)
        .map(|field| String::from_utf8_lossy(field).into_owned())
        .collect();
    let field = |index: usize| fields.get(index).map_or("", String::as_str);
    let side = |mode: usize, id: usize| {
        let mode = field(mode);
        (!mode.bytes().all(|byte| byte == b'0')).then(|| (mode.to_owned(), field(id).to_owned()))
    };
    Listed::Staged {
        head: side(3, 6),
        index: side(4, 7),
        clean: field(1).as_bytes().get(1) == Some(&b'.'),
    }
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
    use super::{Drafted, Listed, changed_paths, edits_in};

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
            .map(|(path, _)| String::from_utf8_lossy(path).into_owned())
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

    fn staged(head: &str, index: &str, clean: bool) -> Listed {
        let side = |id: &str| (!id.is_empty()).then(|| ("100644".to_owned(), id.to_owned()));
        Listed::Staged {
            head: side(head),
            index: side(index),
            clean,
        }
    }

    fn checkpointed() -> Vec<(String, Listed)> {
        vec![
            ("a".to_owned(), staged("h1", "d1", true)),
            ("gone".to_owned(), staged("h2", "", true)),
            ("same".to_owned(), staged("h3", "h3", true)),
        ]
    }

    #[test]
    fn a_checkpoint_records_only_what_the_index_and_working_tree_hold() {
        let mut drafted = Drafted::new();
        drafted.insert("a".to_owned(), Some(("100644".to_owned(), "d1".to_owned())));
        drafted.insert("gone".to_owned(), None);
        assert!(!edits_in(&checkpointed(), &drafted));

        // Each of these is an edit no checkpoint records.
        let executable = Listed::Staged {
            head: Some(("100644".to_owned(), "h1".to_owned())),
            index: Some(("100755".to_owned(), "d1".to_owned())),
            clean: true,
        };
        let changes = [
            (0, staged("h1", "d1", false)),
            (0, staged("h1", "h1", true)),
            (0, Listed::Unmerged),
            (0, executable),
            (1, staged("h2", "h2", true)),
            (2, staged("h3", "x", true)),
            (2, Listed::Untracked),
        ];
        for (index, entry) in changes {
            let mut entries = checkpointed();
            entries[index].1 = entry;
            let text = format!("{entries:?}");
            assert!(edits_in(&entries, &drafted), "{text}");
        }
        let mut unlisted = checkpointed();
        unlisted.remove(1);
        assert!(edits_in(&unlisted, &drafted));
        let mut extra = checkpointed();
        extra.push(("new".to_owned(), Listed::Untracked));
        assert!(edits_in(&extra, &drafted));
    }

    #[test]
    fn a_status_entry_gives_its_head_and_index_sides() {
        let status = b"1 AM N... 000000 100755 100644 0000 bb src/new.sh\0\
            1 D. N... 100644 000000 000000 aa 0000 src/gone.rs\0";
        let entries = changed_paths(status);
        assert_eq!(
            entries[0].1,
            Listed::Staged {
                head: None,
                index: Some(("100755".to_owned(), "bb".to_owned())),
                clean: false,
            }
        );
        assert_eq!(
            entries[1].1,
            Listed::Staged {
                head: Some(("100644".to_owned(), "aa".to_owned())),
                index: None,
                clean: true,
            }
        );
    }
}

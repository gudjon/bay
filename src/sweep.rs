//! G615 (a fleet finding, 2026-10-31: on a hosted remote a `bay status` spent about 7 s of 12 to
//! 19 s on the network: the trunk fetch alone first, then eight fetches and listings beside
//! each other, three of its sets two round trips one after another). A status read now asks the
//! remote once: one `git fetch --dry-run --porcelain` of every namespace the read judges. Git
//! lists each namespace by its prefix, brings the objects of every ref that moved, writes no
//! ref, and names each update it would make. bay then makes only the updates that each
//! namespace's own rule allows, in one ref transaction. When nothing moved, no object comes and
//! no ref is written.
//!
//! G617 (a fleet finding, 2026-10-31: the sweep alone took 3.2 to 4.0 s of each status on a
//! hosted remote, however few refs had moved): a listing's sweep serves every status read of this
//! store for [`BAY_SWEPT_S`] after it. Inside that window a read asks the remote nothing, reads
//! the store as that sweep left it, and names that sweep's time as its `fetched_at`. A push from
//! this store ends the window of every sweep that began before the push ended, so the read after
//! a write asks the remote again.
//!
//! G618 (a fleet finding, 2026-10-31: in a shared store most listings push a chain that local
//! work left ahead of the remote, so the push ended the window of the listing's own sweep, and 3
//! of 4 reads after one asked the remote again): a push ends no window when this store holds what
//! it sent. After it the remote holds this store's value of each pushed ref, so the store is as
//! fresh as the sweep left it, and the write reads back. A push that failed, sent what bay cannot
//! name, or sent a ref the store no longer holds at that value, still ends the window.
//!
//! G620 (a fleet finding, 2026-11-01: at load 100 to 155 a sweep took about 34 s, so its window,
//! 15 s from its start, had closed before it ended, and every read swept again): the window counts
//! from the sweep's end, and it is twice as long as the sweep took, at least [`BAY_SWEPT_S`] and
//! at most [`BAY_SWEPT_MAX_S`]. A store that reads all the time then sweeps about a third of the
//! time (a 34 s sweep serves 60 s after it), and a quick sweep serves as before.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use crate::git::{Git, GitError};

/// The shortest time, in seconds, a listing's sweep serves the status reads of its store after it
/// ends (G620: twice as long as it took, from this to [`BAY_SWEPT_MAX_S`]).
pub const BAY_SWEPT_S: u64 = 15;

/// The longest time, in seconds, a sweep serves the reads after it ends.
pub const BAY_SWEPT_MAX_S: u64 = 60;

use crate::version::TEST_HOOKS;

/// One update the fetch would make to the local ref it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Move {
    /// Git's flag: ' ' a fast-forward, '+' a forced update, '*' a new ref, '-' a prune (the
    /// remote lacks the ref).
    pub flag: char,
    /// The local ref before; `None` when it does not exist.
    pub old: Option<String>,
    /// The remote's; `None` for a prune.
    pub new: Option<String>,
}

/// One ref update [`apply`] makes: the ref, its value now, and its new value (`None` deletes).
pub type Update = (String, Option<String>, Option<String>);

/// What one sweep found.
#[derive(Debug, Default)]
pub struct Swept {
    /// Each local ref that would move, by name.
    pub moves: BTreeMap<String, Move>,
    /// When the remote was asked: taken before the sweep, so it never overstates (G80).
    pub at: String,
    /// The same time, in Unix seconds.
    pub unix: u64,
}

impl Swept {
    /// The remote's value of local ref `name` in a swept namespace, where `local` is this
    /// store's: the value it would move to, else `local`.
    #[must_use]
    pub fn remote_of<'a>(&'a self, name: &str, local: Option<&'a str>) -> Option<&'a str> {
        match self.moves.get(name) {
            Some(found) => found.new.as_deref(),
            None => local,
        }
    }

    /// The moves of the local refs under `prefix`, by name.
    pub fn under<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (&'a str, &'a Move)> {
        self.moves
            .range(prefix.to_owned()..)
            .take_while(move |(name, _)| name.starts_with(prefix))
            .map(|(name, found)| (name.as_str(), found))
    }

    /// The updates of the moves under `prefix` that `take` allows, as [`apply`] makes them.
    #[must_use]
    pub fn taken(&self, prefix: &str, take: impl Fn(&str, &Move) -> bool) -> Vec<Update> {
        self.under(prefix)
            .filter(|(name, found)| take(name, found))
            .map(|(name, found)| (name.to_owned(), found.old.clone(), found.new.clone()))
            .collect()
    }
}

/// Asks `remote` once for every ref `refspecs` name, pruned to what it holds: the objects of
/// each ref that moved come, and no ref is written.
///
/// # Errors
///
/// The remote cannot be read, or lacks a ref a refspec names exactly.
pub fn sweep(git: &Git, remote: &str, refspecs: &[String]) -> Result<Swept, GitError> {
    let unix = crate::live::unix_now();
    let at = crate::clock::rfc3339_of(unix);
    let mut args = vec![
        "fetch",
        "--dry-run",
        "--porcelain",
        "--prune",
        "--no-tags",
        remote,
    ];
    args.extend(refspecs.iter().map(String::as_str));
    let listed = git.run(&args)?;
    Ok(Swept {
        moves: parse(&listed),
        at,
        unix,
    })
}

/// In a test build, the window `BAY_TEST_SWEPT_S` sets, in seconds after the sweep's end,
/// whatever the sweep took.
fn test_window_s() -> Option<u64> {
    std::env::var("BAY_TEST_SWEPT_S")
        .ok()
        .filter(|_| TEST_HOOKS)
        .and_then(|text| text.parse().ok())
}

/// The window after a sweep that took `took` seconds (G620): twice that, from [`BAY_SWEPT_S`] to
/// [`BAY_SWEPT_MAX_S`].
#[must_use]
pub fn window_after(took: u64) -> u64 {
    test_window_s().unwrap_or_else(|| span(took))
}

/// Twice `took`, from [`BAY_SWEPT_S`] to [`BAY_SWEPT_MAX_S`].
fn span(took: u64) -> u64 {
    took.saturating_mul(2).clamp(BAY_SWEPT_S, BAY_SWEPT_MAX_S)
}

/// The longest a sweep serves after it began, in seconds: its fetch's bound and the longest
/// window after it. The push marks are kept this long, so a window never outlives them.
fn served_s() -> u64 {
    let window = test_window_s().unwrap_or(0).max(BAY_SWEPT_MAX_S);
    crate::result::GIT_NETWORK_TIMEOUT.as_secs() + window
}

/// The record of the last listing's sweep of the store whose common directory is `common`.
fn record(common: &Path) -> PathBuf {
    common.join("bay").join("swept.json")
}

/// G619: the lock file of the store's sweep. The one process that sweeps the store holds it for
/// its whole life, and writes in it its pid, its start (`since`) and, as it ends, `ended`.
fn lock_file(common: &Path) -> PathBuf {
    common.join("bay").join("sweep.lock")
}

/// The sweep lock of a store, held while this lives ([`lock`]).
pub struct Lock {
    file: std::fs::File,
    since: u64,
}

impl Lock {
    /// Writes that this sweep has ended, then lets the lock go.
    pub fn end(mut self) {
        let ended = crate::live::unix_now();
        let text = json!({"pid": std::process::id(), "since": self.since, "ended": ended});
        let _ = write_lock(&mut self.file, &text);
    }
}

fn write_lock(file: &mut std::fs::File, text: &Value) -> std::io::Result<()> {
    use std::io::{Seek as _, Write as _};
    file.set_len(0)?;
    file.rewind()?;
    file.write_all(text.to_string().as_bytes())
}

/// Takes the sweep lock of the store whose common directory is `common`: `Ok(Ok(lock))` when
/// this process holds it now, `Ok(Err(running))` when another sweep holds it, with what that
/// sweep wrote in it ([`running`]).
///
/// # Errors
///
/// The lock file cannot be opened or locked.
pub fn lock(common: &Path) -> Result<Result<Lock, Value>, String> {
    let path = lock_file(common);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {
            let since = crate::live::unix_now();
            let _ = write_lock(
                &mut file,
                &json!({"pid": std::process::id(), "since": since}),
            );
            Ok(Ok(Lock { file, since }))
        }
        Err(std::fs::TryLockError::WouldBlock) => Ok(Err(running(common))),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(format!("cannot lock {}: {error}", path.display()))
        }
    }
}

/// What the store's last sweep wrote in its lock file: `{pid, since}`, and `ended` once it has
/// ended; `null` when nothing readable is there (a sweep that is writing it, or none ever ran).
#[must_use]
pub fn running(common: &Path) -> Value {
    std::fs::read_to_string(lock_file(common))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null)
}

/// The last sweep this store recorded ([`remember`]), whatever its window: its time `at` and its
/// start `unix`.
#[must_use]
pub fn last(common: &Path) -> Option<(String, u64)> {
    let text = std::fs::read_to_string(record(common)).ok()?;
    let held: Value = serde_json::from_str(&text).ok()?;
    Some((held["at"].as_str()?.to_owned(), held["unix"].as_u64()?))
}

/// The marks of the store's pushes ([`pushed`]), one file each, named `<unix>-<pid>-<n>`.
fn push_marks(common: &Path) -> PathBuf {
    common.join("bay").join("swept-pushes")
}

/// The Unix time a push mark's file `name` starts with. A file of a mark being written has a
/// `.` in its name.
fn mark_time(name: &std::ffi::OsStr) -> Option<u64> {
    name.to_str()?.split(['-', '.']).next()?.parse().ok()
}

/// The time of the last listing's sweep of `remote` by `refspecs` in the store of `git`, whose
/// common directory is `common`, while its window is open at `now` ([`window_after`] its end);
/// `None` when no such sweep is that recent, or the store pushed after it began what it does not
/// hold ([`holds_pushed`]). A record that names no end opens no window.
#[must_use]
pub fn window(
    git: &Git,
    common: &Path,
    remote: &str,
    refspecs: &[String],
    now: u64,
) -> Option<String> {
    let text = std::fs::read_to_string(record(common)).ok()?;
    let held: Value = serde_json::from_str(&text).ok()?;
    let unix = held["unix"].as_u64()?;
    let ended = held["ended"].as_u64()?;
    let open = held["remote"] == remote
        && held["refspecs"] == json!(refspecs)
        && unix <= now
        && now < ended.saturating_add(window_after(ended.saturating_sub(unix)))
        && now < unix.saturating_add(served_s())
        && holds_pushed(git, common, refspecs, unix);
    open.then(|| held["at"].as_str().map(str::to_owned))?
}

/// Whether the store of `git` holds what each of its pushes since `unix` sent (G618): each ref a
/// push sent is, as the ref the sweep's `refspecs` map it to, at the object the push sent. A ref
/// no refspec maps is one the read does not judge. A push that failed, or whose mark cannot be
/// read, holds nothing.
fn holds_pushed(git: &Git, common: &Path, refspecs: &[String], unix: u64) -> bool {
    let entries = match std::fs::read_dir(push_marks(common)) {
        Ok(entries) => entries,
        Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
    };
    // Each local ref, and the object a push sent to the ref it copies.
    let mut wanted: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().contains('.') || mark_time(&name).is_none_or(|at| at < unix) {
            continue;
        }
        let mark = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let Some(sent) = mark.as_ref().and_then(|mark| mark["refs"].as_array()) else {
            return false;
        };
        for pair in sent {
            let (Some(source), Some(target)) = (pair[0].as_str(), pair[1].as_str()) else {
                return false;
            };
            let Some(local) = local_of(refspecs, target) else {
                continue;
            };
            if is_oid(source) {
                wanted.push((local, source.to_owned()));
            } else if source != target || local != target {
                return false;
            }
        }
    }
    if wanted.is_empty() {
        return true;
    }
    let mut args = vec!["for-each-ref", "--format=%(objectname) %(refname)"];
    args.extend(wanted.iter().map(|(local, _)| local.as_str()));
    let Ok(listed) = git.run(&args) else {
        return false;
    };
    let held: BTreeMap<&str, &str> = listed
        .lines()
        .filter_map(|line| line.split_once(' ').map(|(oid, name)| (name, oid)))
        .collect();
    wanted
        .iter()
        .all(|(local, oid)| held.get(local.as_str()) == Some(&oid.as_str()))
}

/// The local ref a refspec of `refspecs` copies the remote's ref `name` to, if one does.
fn local_of(refspecs: &[String], name: &str) -> Option<String> {
    refspecs.iter().find_map(|spec| {
        let (remote, local) = spec.trim_start_matches('+').split_once(':')?;
        match (remote.strip_suffix('*'), local.strip_suffix('*')) {
            (Some(remote), Some(local)) => name
                .strip_prefix(remote)
                .map(|rest| format!("{local}{rest}")),
            (None, None) => (name == remote).then(|| local.to_owned()),
            _ => None,
        }
    })
}

/// Whether `text` is a full object id.
fn is_oid(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The refs `git push <args>` sent, as pairs of the source and the remote's ref; `None` when the
/// push may have changed a ref no pair names: it names no refspec, deletes, or takes an option
/// bay does not pass.
fn pushed_refs(args: &[&str]) -> Option<Vec<(String, String)>> {
    let mut words = args.iter().filter(|word| {
        !matches!(**word, "-q" | "--atomic") && !word.starts_with("--force-with-lease=")
    });
    let remote = words.next()?;
    if remote.starts_with('-') {
        return None;
    }
    let sent = words
        .map(|spec| {
            let (source, target) = spec.strip_prefix('+').unwrap_or(spec).split_once(':')?;
            (!source.is_empty() && target.starts_with("refs/"))
                .then(|| (source.to_owned(), target.to_owned()))
        })
        .collect::<Option<Vec<_>>>()?;
    (!sent.is_empty()).then_some(sent)
}

/// Records that a listing's sweep of `remote` by `refspecs` settled this store, now, so the reads
/// in its window are served by it: its start and its end. Best effort: a record that cannot be
/// written opens no window.
pub fn remember(common: &Path, remote: &str, refspecs: &[String], swept: &Swept) {
    let path = record(common);
    let ended = crate::live::unix_now().max(swept.unix);
    let held = json!({
        "remote": remote,
        "refspecs": refspecs,
        "at": swept.at,
        "unix": swept.unix,
        "ended": ended,
    });
    replace(&path, &held.to_string());
}

/// Records that the store whose common directory is `common` ran `git push <args>` at `now`, `ok`
/// when it succeeded: a push may have changed what the remote holds, so it ends the window of
/// every sweep that began at or before it, unless the store holds what it sent
/// ([`holds_pushed`]). A push that ends inside the second a sweep began counts for that sweep
/// too. The marks older than any window ([`served_s`]) are removed. Best effort.
pub fn pushed(common: &Path, now: u64, args: &[&str], ok: bool) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = push_marks(common);
    let past = now.saturating_sub(served_s());
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        if mark_time(&entry.file_name()).is_some_and(|at| at < past) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let sent = if ok { pushed_refs(args) } else { None };
    let name = format!(
        "{now}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    replace(&dir.join(name), &json!({ "refs": sent }).to_string());
}

/// Writes `text` at `path` whole, through a file of this process beside it. Best effort.
fn replace(path: &Path, text: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, text).is_ok() {
        let _ = crate::durable::rename(&tmp, path);
    }
}

/// The moves of `git fetch --porcelain`: lines of a flag, the old and new object ids, and the
/// local ref.
fn parse(listed: &str) -> BTreeMap<String, Move> {
    let id = |oid: &str| (!oid.bytes().all(|byte| byte == b'0')).then(|| oid.to_owned());
    listed
        .lines()
        .filter_map(|line| {
            let flag = line.chars().next()?;
            let mut words = line.get(1..)?.split_whitespace();
            let (old, new, name) = (words.next()?, words.next()?, words.next()?);
            Some((
                name.to_owned(),
                Move {
                    flag,
                    old: id(old),
                    new: id(new),
                },
            ))
        })
        .collect()
}

/// Makes each move of `moves` (a ref, its value now, its new value; `None` deletes) in one
/// transaction, each compared with its value now. When the transaction fails (a ref another
/// process moved meanwhile), each is made alone, and one that moved meanwhile is left.
pub fn apply(git: &Git, moves: &[Update]) {
    let line = |text: &mut String, (name, old, new): &Update| {
        let _ = match (old, new) {
            (Some(old), Some(new)) => writeln!(text, "update {name} {new} {old}"),
            (None, Some(new)) => writeln!(text, "create {name} {new}"),
            (Some(old), None) => writeln!(text, "delete {name} {old}"),
            (None, None) => Ok(()),
        };
    };
    let mut all = String::new();
    for found in moves {
        line(&mut all, found);
    }
    if all.is_empty()
        || git
            .run_with(&["update-ref", "--stdin"], &[], Some(all.as_bytes()))
            .is_ok()
    {
        return;
    }
    for found in moves {
        let mut one = String::new();
        line(&mut one, found);
        let _ = git.run_with(&["update-ref", "--stdin"], &[], Some(one.as_bytes()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_is_twice_the_sweep_from_its_floor_to_its_ceiling() {
        assert_eq!(span(0), BAY_SWEPT_S);
        assert_eq!(span(7), BAY_SWEPT_S);
        assert_eq!(span(12), 24);
        assert_eq!(span(34), BAY_SWEPT_MAX_S);
        assert_eq!(span(u64::MAX), BAY_SWEPT_MAX_S);
    }

    #[test]
    fn a_push_names_the_refs_it_sent_or_none() {
        let a = "a".repeat(40);
        let chain = "refs/bay/changes/c1";
        let lease = format!("--force-with-lease={chain}:");
        let sent = format!("{chain}:{chain}");
        let request = format!("+{a}:refs/bay/requests/c1");
        assert_eq!(
            pushed_refs(&["-q", "--atomic", &lease, "origin", &sent, &request]),
            Some(vec![
                (chain.to_owned(), chain.to_owned()),
                (a.clone(), "refs/bay/requests/c1".to_owned())
            ])
        );
        assert_eq!(
            pushed_refs(&["-q", "--delete", "origin", "refs/heads/b"]),
            None
        );
        assert_eq!(pushed_refs(&["-q", "origin", ":refs/heads/b"]), None);
        assert_eq!(pushed_refs(&["-q", "origin"]), None);
        assert_eq!(pushed_refs(&["-q", "origin", "HEAD"]), None);
    }

    #[test]
    fn a_refspec_maps_a_remote_ref_to_its_local_copy() {
        let refspecs = [
            "+refs/heads/main:refs/remotes/origin/main".to_owned(),
            "+refs/bay/changes/*:refs/bay/changes/*".to_owned(),
            "+refs/bay/live/*:refs/bay/fleet/*".to_owned(),
        ];
        let local = |name| local_of(&refspecs, name);
        assert_eq!(
            local("refs/heads/main").as_deref(),
            Some("refs/remotes/origin/main")
        );
        assert_eq!(local("refs/heads/mainline"), None);
        assert_eq!(
            local("refs/bay/changes/c1").as_deref(),
            Some("refs/bay/changes/c1")
        );
        assert_eq!(
            local("refs/bay/live/a").as_deref(),
            Some("refs/bay/fleet/a")
        );
        assert_eq!(local("refs/bay/intents/x"), None);
        assert!(is_oid(&"f".repeat(64)) && !is_oid("refs/heads/main"));
    }

    #[test]
    fn a_porcelain_listing_names_each_move_with_its_flag() {
        let zero = "0".repeat(40);
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        let listed = format!(
            "  {a} {b} refs/bay/changes/c1\n+ {a} {b} refs/bay/log/own\n* {zero} {b} refs/remotes/origin/dev\n- {a} {zero} refs/bay/fleet/gone"
        );
        let moves = parse(&listed);
        assert_eq!(moves["refs/bay/changes/c1"].flag, ' ');
        assert_eq!(moves["refs/bay/log/own"].flag, '+');
        assert_eq!(moves["refs/remotes/origin/dev"].old, None);
        assert_eq!(moves["refs/bay/fleet/gone"].new, None);
        let swept = Swept {
            moves,
            at: String::new(),
            unix: 0,
        };
        assert_eq!(swept.remote_of("refs/bay/fleet/gone", Some(&a)), None);
        assert_eq!(
            swept.remote_of("refs/bay/changes/c2", Some(&a)),
            Some(a.as_str())
        );
        assert_eq!(swept.under("refs/bay/fleet/").count(), 1);
        assert_eq!(
            swept.taken("refs/bay/", |_, found| found.flag != '-'),
            vec![
                (
                    "refs/bay/changes/c1".to_owned(),
                    Some(a.clone()),
                    Some(b.clone())
                ),
                (
                    "refs/bay/log/own".to_owned(),
                    Some(a.clone()),
                    Some(b.clone())
                )
            ]
        );
    }
}

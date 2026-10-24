//! Locks across machines (BACKLOG 57, owner ruling 2026-10-23): `[locks]` in `.bay/policy.toml`.
//!
//! Some files never merge: an image, a model, a binary. Two machines that edit one lose one
//! machine's work at merge. `[locks] paths` names them by glob (the `when_paths` globs,
//! `crate::policy::glob_matches`), judged by the policy at main's tip as this clone last
//! fetched it, so a change never lifts a lock for itself.
//!
//! At `tool.before`, the first edit of a lockable path takes its lock on the remote: a ref
//! `refs/bay/locks/<hash>` (the path's sha256, 32 hex characters) pointing at a commit whose
//! `lock.json` names the path, the actor, the session, the change and the time it was taken
//! (`bay.lock/1`). The push is create-only (`--force-with-lease=<ref>:`), so of two machines
//! that edit one path at once, exactly one takes it. The other is refused `PATH_HELD`, naming
//! the holder and how long it has held the lock. A lock this session holds lets its edits
//! through. A lock older than the takeover window is taken over, with a lease on the commit it
//! names, as are a lock bay cannot read and a lock with no session to end it.
//!
//! The session's steps renew its locks once they are half the window old, and its stop (not a
//! refused one, ruling R7) and its end release them, as they release its claims
//! (`crate::claims`). Offline, a lockable edit is refused `REMOTE_UNREACHABLE`: unknown is
//! never free. A shell command's writes are not judged here (`CANNOT_SEE`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::repo::Repo;

pub const SCHEMA: &str = "bay.lock/1";
/// The file a lock's commit holds.
const FILE: &str = "lock.json";
/// The file in a session's state directory that lists the locks it took, one `<ref> <commit>`
/// per line.
const STATE_FILE: &str = "locks";

/// The `[locks]` table.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Locks {
    /// The lockable paths, as globs.
    #[serde(default)]
    pub paths: Vec<String>,
}

impl Locks {
    /// A validity rule of the policy (`POLICY_INVALID`): each glob names something.
    ///
    /// # Errors
    ///
    /// Why the table is invalid.
    pub fn validate(&self) -> Result<(), String> {
        if self.paths.iter().any(|glob| glob.trim().is_empty()) {
            return Err("[locks] paths holds an empty glob".to_owned());
        }
        Ok(())
    }

    /// Whether `path` is lockable.
    #[must_use]
    pub fn covers(&self, path: &str) -> bool {
        self.paths
            .iter()
            .any(|glob| crate::policy::glob_matches(glob, path))
    }
}

/// One lock, as its commit's `lock.json` holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    pub schema: String,
    pub path: String,
    pub actor: String,
    pub session: Option<String>,
    pub change: Option<String>,
    /// When it was taken, last renewed or released, in Unix seconds.
    pub at_s: u64,
    /// Whether the session released it: the ref stays and keeps its holder, so the change still
    /// holds it for the merge guard ([`held_by`]) until another takes it, and it is free to take.
    #[serde(default)]
    pub released: bool,
}

/// Why a lockable edit is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Another session holds the path's lock.
    Held { lock: Box<Lock>, age_s: u64 },
    /// The remote did not answer, so whether the path is free is unknown.
    Unreachable { path: String, why: String },
    /// The successor rule: main changed the path after this work's base, so an edit from here
    /// would drop that change. `last` is main's last commit to it.
    Stale { path: String, last: String },
}

/// The ref a lock of `path` lives at.
#[must_use]
pub fn lock_ref(path: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(path.as_bytes());
    format!(
        "{}{}",
        crate::names::LOCKS,
        &crate::agent_settings::hex(&digest)[..32]
    )
}

/// `tool.before` on an edit tool: takes the lock of each of `targets` (absolute, or relative to
/// `cwd`) the policy names lockable, for `session` of this actor at `now`. A target outside a
/// repository bay opens, or not lockable, is not judged here.
///
/// # Errors
///
/// The first target whose lock another session holds, or whose remote did not answer.
pub fn take(
    cwd: &Path,
    targets: &[PathBuf],
    session: Option<&str>,
    now: u64,
) -> Result<(), Refused> {
    take_for(cwd, targets, session, None, now)
}

/// [`take`], the lock commit naming `change` when it is given (a save names the change it
/// made), else the change of the view the target is in. The merge guard counts only the
/// current holder's lock commits that name the change it judges ([`held_by`]).
///
/// # Errors
///
/// As [`take`].
pub fn take_for(
    cwd: &Path,
    targets: &[PathBuf],
    session: Option<&str>,
    change: Option<&str>,
    now: u64,
) -> Result<(), Refused> {
    for target in targets {
        let Some(target) = crate::coord::resolve(&cwd.join(target)) else {
            continue;
        };
        let Some((root, path)) = crate::claims::tree_path(&target) else {
            continue;
        };
        let Ok(repo) = Repo::open_known(&root) else {
            continue;
        };
        let Ok(locks) = crate::policy::locks_at(&repo.git, &repo.trunk_tracking_ref()) else {
            continue;
        };
        if !locks.covers(&path) {
            continue;
        }
        successor(&repo, &root, &path)?;
        let me = Lock {
            schema: SCHEMA.to_owned(),
            path,
            actor: crate::change::actor_name(),
            session: session.map(str::to_owned),
            change: change
                .map(str::to_owned)
                .or_else(|| repo.view_change(&root)),
            at_s: now,
            released: false,
        };
        take_one(&repo, &me, now)?;
    }
    Ok(())
}

/// The successor rule (BACKLOG 57): the work in the tree at `root` may take `path`'s lock only
/// when its base holds main's last change to the path, as main stands on the remote now.
fn successor(repo: &Repo, root: &Path, path: &str) -> Result<(), Refused> {
    let main = repo.fetch_trunk().map_err(|error| Refused::Unreachable {
        path: path.to_owned(),
        why: error.message(),
    })?;
    let last = repo
        .git
        .run(&["log", "-1", "--format=%H", &main, "--", path])
        .unwrap_or_default();
    if last.is_empty() {
        return Ok(());
    }
    let base = crate::git::Git::new(root)
        .run(&["rev-parse", "HEAD"])
        .unwrap_or_default();
    if !base.is_empty() && repo.git.is_ancestor(&last, &base).unwrap_or(false) {
        return Ok(());
    }
    Err(Refused::Stale {
        path: path.to_owned(),
        last,
    })
}

/// A lock refusal as a command's result (`bay save`): `PATH_HELD` naming the holder,
/// `REMOTE_UNREACHABLE`, or `INPUT_CHANGED` when main changed the path after this work began.
#[must_use]
pub fn refusal(refused: Refused) -> crate::result::BayResult {
    use crate::result::{BayResult, Reason};
    use serde_json::json;
    match refused {
        Refused::Held { lock, age_s } => BayResult::blocked(Reason::PathHeld).with_detail(json!({
            "paths": [lock.path],
            "holder": lock.session,
            "actor": lock.actor,
            "held_s": age_s,
        })),
        Refused::Unreachable { path, why } => BayResult::blocked(Reason::RemoteUnreachable)
            .with_detail(json!({"paths": [path], "why": why})),
        Refused::Stale { path, last } => BayResult::blocked(Reason::InputChanged).with_detail(
            json!({"paths": [path], "main_commit": last,
                "why": "main changed this lockable file after this copy was taken; get the newest main first"}),
        ),
    }
}

/// What the remote holds at a lock's ref.
enum Remote {
    Free,
    /// The commit, and its lock when bay can read it.
    Taken(String, Option<Lock>),
}

/// Reads the lock at `name` on the remote.
fn read_remote(repo: &Repo, name: &str, path: &str) -> Result<Remote, Refused> {
    let unreachable = |why: String| Refused::Unreachable {
        path: path.to_owned(),
        why,
    };
    let (code, out, err) = repo
        .git
        .run_status(&["ls-remote", "--exit-code", repo.remote(), name])
        .map_err(|error| unreachable(error.message()))?;
    match code {
        0 => {}
        2 => return Ok(Remote::Free),
        _ => return Err(unreachable(err)),
    }
    let commit = out.split_whitespace().next().unwrap_or_default().to_owned();
    repo.git
        .run(&["fetch", "-q", repo.remote(), &format!("+{name}:{name}")])
        .map_err(|error| unreachable(error.message()))?;
    let lock = repo
        .git
        .file_at(&commit, FILE)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<Lock>(&text).ok())
        .filter(|lock| lock.schema == SCHEMA);
    Ok(Remote::Taken(commit, lock))
}

/// Takes `me`'s lock, or keeps it when this session holds it already.
fn take_one(repo: &Repo, me: &Lock, now: u64) -> Result<(), Refused> {
    let name = lock_ref(&me.path);
    let window_s = repo.config.takeover().as_secs();
    let lease = match read_remote(repo, &name, &me.path)? {
        Remote::Free => String::new(),
        Remote::Taken(commit, Some(lock)) if lock.released => commit,
        Remote::Taken(commit, Some(lock)) if mine(&lock, me) => {
            // Kept as it is while young, unless it is to name a change it does not name yet.
            if now.saturating_sub(lock.at_s) * 2 < window_s && lock.change == me.change {
                return Ok(());
            }
            commit
        }
        Remote::Taken(_, Some(lock))
            if lock.session.is_some() && now.saturating_sub(lock.at_s) < window_s =>
        {
            let age_s = now.saturating_sub(lock.at_s);
            return Err(Refused::Held {
                lock: Box::new(lock),
                age_s,
            });
        }
        // Past the window, unreadable, or with no session to end it: taken over.
        Remote::Taken(commit, _) => commit,
    };
    if push(repo, &name, &lease, me).is_ok() {
        return Ok(());
    }
    // Another machine took it between the read and the push, or the remote stopped answering.
    match read_remote(repo, &name, &me.path)? {
        Remote::Taken(_, Some(lock)) if !mine(&lock, me) => {
            let age_s = now.saturating_sub(lock.at_s);
            Err(Refused::Held {
                lock: Box::new(lock),
                age_s,
            })
        }
        _ => Err(Refused::Unreachable {
            path: me.path.clone(),
            why: format!("the push of {name} was refused"),
        }),
    }
}

/// Whether `lock` is the one `me` would take: the same actor and session.
fn mine(lock: &Lock, me: &Lock) -> bool {
    lock.actor == me.actor && lock.session.is_some() && lock.session == me.session
}

/// The commit of `lock` on `parent` (none for a new ref), so the ref's history keeps every
/// holder.
fn lock_commit(repo: &Repo, lock: &Lock, parent: &str) -> Result<String, String> {
    let fault = |error: crate::git::GitError| error.message();
    let mut text = serde_json::to_string_pretty(lock).map_err(|error| error.to_string())?;
    text.push('\n');
    let blob = repo
        .git
        .run_with(
            &["hash-object", "-w", "--stdin"],
            &[],
            Some(text.as_bytes()),
        )
        .map_err(fault)?;
    let entry = format!("100644 blob {blob}\t{FILE}\n");
    let tree = repo
        .git
        .run_with(&["mktree"], &[], Some(entry.as_bytes()))
        .map_err(fault)?;
    let message = format!(
        "{} {}",
        if lock.released { "release" } else { "lock" },
        lock.path
    );
    let mut args = vec!["commit-tree", tree.as_str()];
    if !parent.is_empty() {
        args.extend(["-p", parent]);
    }
    args.extend(["--no-gpg-sign", "-m", message.as_str()]);
    repo.git
        .run_with(&args, crate::change::RECORD_IDENTITY, None)
        .map_err(fault)
}

/// Pushes `lock` to `name` with a lease on `lease` (empty: no ref), its commit on `lease`, and
/// remembers it in the session's state directory while it is held.
fn push(repo: &Repo, name: &str, lease: &str, lock: &Lock) -> Result<(), String> {
    let commit = lock_commit(repo, lock, lease)?;
    repo.git
        .run(&[
            "push",
            "-q",
            &format!("--force-with-lease={name}:{lease}"),
            repo.remote(),
            &format!("{commit}:{name}"),
        ])
        .map_err(|error| error.message())?;
    let _ = repo.git.run(&["update-ref", name, &commit]);
    if let Some(session) = lock
        .session
        .as_deref()
        .and_then(crate::session::SessionId::parse)
        .filter(|_| !lock.released)
    {
        remember(&crate::session::state_dir(repo, &session), name, &commit);
    }
    Ok(())
}

/// The locks `state_dir` lists: each ref and the commit this session pushed to it.
fn remembered(state_dir: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(state_dir.join(STATE_FILE))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, commit)| (name.to_owned(), commit.to_owned()))
        .collect()
}

fn write_remembered(state_dir: &Path, locks: &[(String, String)]) {
    if locks.is_empty() {
        let _ = std::fs::remove_file(state_dir.join(STATE_FILE));
        return;
    }
    let text = locks
        .iter()
        .fold(String::new(), |mut text, (name, commit)| {
            let _ = writeln!(text, "{name} {commit}");
            text
        });
    if std::fs::create_dir_all(state_dir).is_ok() {
        let _ = std::fs::write(state_dir.join(STATE_FILE), text);
    }
}

fn remember(state_dir: &Path, name: &str, commit: &str) {
    let mut locks: Vec<(String, String)> = remembered(state_dir)
        .into_iter()
        .filter(|(known, _)| known != name)
        .collect();
    locks.push((name.to_owned(), commit.to_owned()));
    write_remembered(state_dir, &locks);
}

/// A step of the session: renews each lock it holds that is half the takeover window old, with
/// a lease on the commit it pushed. A lock another session took over is forgotten.
pub fn renew(repo: &Repo, state_dir: &Path, now: u64) {
    let window_s = repo.config.takeover().as_secs();
    for (name, commit) in remembered(state_dir) {
        let Some(lock) = repo
            .git
            .file_at(&commit, FILE)
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str::<Lock>(&text).ok())
        else {
            continue;
        };
        if now.saturating_sub(lock.at_s) * 2 < window_s {
            continue;
        }
        let renewed = Lock { at_s: now, ..lock };
        if push(repo, &name, &commit, &renewed).is_err() {
            let kept: Vec<(String, String)> = remembered(state_dir)
                .into_iter()
                .filter(|(known, _)| *known != name)
                .collect();
            write_remembered(state_dir, &kept);
        }
    }
}

/// The session's stop or end: releases each lock it holds on the remote, a commit marked
/// released on the one it pushed, with a lease on it, so a lock another session took over is
/// left alone and the ref keeps who held it.
pub fn release(repo: &Repo, state_dir: &Path, now: u64) {
    for (name, commit) in remembered(state_dir) {
        let Some(lock) = lock_at(repo, &commit) else {
            continue;
        };
        let released = Lock {
            at_s: now,
            released: true,
            ..lock
        };
        let _ = push(repo, &name, &commit, &released);
    }
    write_remembered(state_dir, &[]);
}

fn lock_at(repo: &Repo, commit: &str) -> Option<Lock> {
    repo.git
        .file_at(commit, FILE)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<Lock>(&text).ok())
        .filter(|lock| lock.schema == SCHEMA)
}

/// Whether change `change` holds `path`'s lock: the current holder's commits, from the lock
/// ref's tip (as this clone fetched it) back to the first commit of another holder, name it. A
/// release keeps its holder, so a change that released its lock still holds it until someone
/// else takes it; a lock it held before another session took it over counts for nothing.
#[must_use]
pub fn held_by(repo: &Repo, path: &str, change: &str) -> bool {
    let name = lock_ref(path);
    let Ok(history) = repo.git.run(&["rev-list", "--first-parent", &name]) else {
        return false;
    };
    let mut locks = history
        .lines()
        .filter_map(|commit| lock_at(repo, commit))
        .filter(|lock| lock.path == path);
    let Some(tip) = locks.next() else {
        return false;
    };
    let holder = |lock: &Lock| (lock.actor.clone(), lock.session.clone());
    let current = holder(&tip);
    std::iter::once(tip)
        .chain(locks)
        .take_while(|lock| holder(lock) == current)
        // Only a commit that names this very change: an actor is a machine's user, shared by
        // every agent and person on it, so it never stands for the change (the Mac's review of
        // 0.4.0, finding A).
        .any(|lock| lock.change.as_deref() == Some(change))
}

/// The merge guard (BACKLOG 57): the lockable paths, under the policy at `trunk`, that `tree`
/// changes from `trunk` and whose lock change `change` never held. The lock
/// refs are fetched first; a fetch that fails holds nothing, so the guard refuses, never
/// passes, when it cannot tell.
///
/// # Errors
///
/// The policy or the diff cannot be read.
pub fn not_held(repo: &Repo, trunk: &str, tree: &str, change: &str) -> Result<Vec<String>, String> {
    let locks = crate::policy::locks_at(&repo.git, trunk)?;
    if locks.paths.is_empty() {
        return Ok(Vec::new());
    }
    let changed = repo
        .git
        .run(&["diff", "--name-only", "--no-renames", "-z", trunk, tree])
        .map_err(|error| error.message())?;
    let lockable: Vec<&str> = changed
        .split('\0')
        .filter(|path| !path.is_empty() && locks.covers(path))
        .collect();
    if lockable.is_empty() {
        return Ok(Vec::new());
    }
    let _ = repo.git.run(&[
        "fetch",
        "-q",
        "--no-tags",
        repo.remote(),
        &format!("+{}*:{}*", crate::names::LOCKS, crate::names::LOCKS),
    ]);
    Ok(lockable
        .into_iter()
        .filter(|path| !held_by(repo, path, change))
        .map(str::to_owned)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{Locks, lock_ref};

    #[test]
    fn a_lock_ref_is_a_bay_ref_named_by_the_path() {
        let name = lock_ref("art/logo.psd");
        assert!(crate::names::bay_ref(&name).is_ok(), "{name}");
        assert_ne!(name, lock_ref("art/logo2.psd"));
    }

    #[test]
    fn the_globs_name_the_lockable_paths() {
        let locks = Locks {
            paths: vec!["*.psd".to_owned(), "models/".to_owned()],
        };
        assert!(locks.covers("logo.psd"));
        assert!(locks.covers("models/big.bin"));
        assert!(!locks.covers("src/a.rs"));
        assert!(Locks::default().validate().is_ok());
        assert!(
            Locks {
                paths: vec![String::new()]
            }
            .validate()
            .is_err()
        );
    }
}

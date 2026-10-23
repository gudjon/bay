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
    /// When it was taken or last renewed, in Unix seconds.
    pub at_s: u64,
}

/// Why a lockable edit is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Another session holds the path's lock.
    Held { lock: Box<Lock>, age_s: u64 },
    /// The remote did not answer, so whether the path is free is unknown.
    Unreachable { path: String, why: String },
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
        let me = Lock {
            schema: SCHEMA.to_owned(),
            path,
            actor: crate::change::actor_name(),
            session: session.map(str::to_owned),
            change: repo.view_change(&root),
            at_s: now,
        };
        take_one(&repo, &me, now)?;
    }
    Ok(())
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
        Remote::Taken(commit, Some(lock)) if mine(&lock, me) => {
            if now.saturating_sub(lock.at_s) * 2 < window_s {
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

/// Pushes `lock` to `name` with a lease on `lease` (empty: no ref), and remembers it in the
/// session's state directory.
fn push(repo: &Repo, name: &str, lease: &str, lock: &Lock) -> Result<(), String> {
    let body = serde_json::to_value(lock).map_err(|error| error.to_string())?;
    let commit =
        crate::queue::commit(&repo.git, None, FILE, &body, &format!("lock {}", lock.path))?;
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

/// The session's stop or end: deletes each lock it holds on the remote, with a lease on the
/// commit it pushed, so a lock another session took over is left alone.
pub fn release(repo: &Repo, state_dir: &Path) {
    for (name, commit) in remembered(state_dir) {
        let _ = repo.git.run(&[
            "push",
            "-q",
            &format!("--force-with-lease={name}:{commit}"),
            repo.remote(),
            &format!(":{name}"),
        ]);
        let _ = repo.git.run(&["update-ref", "-d", &name, &commit]);
    }
    write_remembered(state_dir, &[]);
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

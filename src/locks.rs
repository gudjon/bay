//! Locks across machines (BACKLOG 57, owner ruling 2026-10-23; BACKLOG 65, 2026-10-25): the
//! `lockable` attribute in `.gitattributes`.
//!
//! Some files never merge: an image, a model, a binary. Two machines that edit one lose one
//! machine's work at merge. Git LFS's `lockable` attribute names them ([`lockable`]), read at
//! main's tip as this clone last fetched it, so a change never lifts a lock for itself.
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
//!
//! Under a key registry that signs every record, a lock commit is signed by this machine's agent
//! key (BACKLOG 57: "a signed, create-only hold ref"), and only a lock commit signed by the agent
//! key enrolled for the actor it names counts: an unsigned one is a lock bay cannot read, so it
//! holds nothing at merge and is taken over at an edit. A bay host refuses one outright.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::registry::{KeyClass, Registry};
use crate::repo::Repo;

pub const SCHEMA: &str = "bay.lock/1";
/// The file a lock's commit holds.
const FILE: &str = "lock.json";
/// The file in a session's state directory that lists the locks it took, one `<ref> <commit>`
/// per line.
const STATE_FILE: &str = "locks";

/// The paths of `paths` that are lockable at `commit` (BACKLOG 65, owner ruling 2026-10-25):
/// Git LFS's `lockable` attribute in `.gitattributes`, read with `git check-attr --source`, so the
/// class of a path has one home on main and a change's own attributes never judge it. A path is
/// lockable when the attribute is set (`lockable`, or `lockable=true`).
///
/// # Errors
///
/// A git fault: unknown is never "not lockable".
pub fn lockable(
    git: &crate::git::Git,
    commit: &str,
    paths: &[&str],
) -> Result<Vec<String>, String> {
    attribute_set(git, commit, "lockable", paths)
}

/// The paths of `paths` whose attribute `name` is set (or `true`) at `commit`, by the
/// `.gitattributes` files of `commit`'s tree.
///
/// # Errors
///
/// A git fault.
pub fn attribute_set(
    git: &crate::git::Git,
    commit: &str,
    name: &str,
    paths: &[&str],
) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    let out = git
        .run_with(
            &["check-attr", "--source", commit, "-z", "--stdin", name],
            &[],
            Some(&input),
        )
        .map_err(|error| error.message())?;
    let words: Vec<&str> = out.split('\0').collect();
    Ok(words
        .chunks(3)
        .filter(|row| row.len() == 3 && matches!(row[2], "set" | "true"))
        .map(|row| row[0].to_owned())
        .collect())
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

/// The locks held now, as this clone last fetched them (BACKLOG 61 b): each lock ref whose tip
/// is not released, with its path, holder (actor and session), change and age. The ref is the
/// state; nothing else records it.
#[must_use]
pub fn held(repo: &Repo, now: u64) -> Vec<serde_json::Value> {
    let Ok(listed) = repo.git.run(&[
        "for-each-ref",
        "--format=%(objectname)",
        crate::names::LOCKS,
    ]) else {
        return Vec::new();
    };
    let registry = signing_registry(repo);
    let mut held: Vec<serde_json::Value> = listed
        .lines()
        .filter_map(|commit| lock_at(repo, registry.as_ref(), commit))
        .filter(|lock| !lock.released)
        .map(|lock| {
            serde_json::json!({
                "path": lock.path,
                "actor": lock.actor,
                "session": lock.session,
                "change": lock.change,
                "held_s": now.saturating_sub(lock.at_s),
            })
        })
        .collect();
    held.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    held
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
        let Ok(lockable) = lockable(&repo.git, &repo.trunk_tracking_ref(), &[path.as_str()]) else {
            continue;
        };
        if lockable.is_empty() {
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
    let lock = lock_at(repo, signing_registry(repo).as_ref(), &commit);
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
    let why = match push(repo, &name, &lease, me) {
        Ok(()) => return Ok(()),
        Err(why) => why,
    };
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
            why: format!("the push of {name} was refused: {why}"),
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
    let key = match signing_registry(repo) {
        Some(_) => Some(crate::signing::Keys::load(&repo.git).agent.ok_or(
            "the repository has a key registry and this machine has no agent key (git config bay.agentKey) to sign the lock with",
        )?),
        None => None,
    };
    let mut args: Vec<String> = Vec::new();
    if let Some(key) = &key {
        args.extend([
            "-c".to_owned(),
            "gpg.format=ssh".to_owned(),
            "-c".to_owned(),
            format!("user.signingkey={}", key.display()),
        ]);
    }
    args.extend(["commit-tree".to_owned(), tree]);
    if !parent.is_empty() {
        args.extend(["-p".to_owned(), parent.to_owned()]);
    }
    args.push(if key.is_some() { "-S" } else { "--no-gpg-sign" }.to_owned());
    args.extend(["-m".to_owned(), message]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    repo.git
        .run_with(&args, crate::change::RECORD_IDENTITY, None)
        .map_err(fault)
}

/// The repository's key registry, when it signs every record (`[policy] signing = "all"`): then
/// a lock commit is signed, and only a signed one counts. `None` without one, or when it cannot
/// be read.
fn signing_registry(repo: &Repo) -> Option<Registry> {
    Registry::current(&repo.git)
        .ok()
        .flatten()
        .filter(Registry::signs_all)
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
        // The commit this session pushed itself: read, not judged.
        let Some(lock) = lock_at(repo, None, &commit) else {
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

fn lock_at(repo: &Repo, registry: Option<&Registry>, commit: &str) -> Option<Lock> {
    signed_lock(&repo.git, registry, commit)
}

/// The lock commit `commit` holds; under `registry`, only when the agent key enrolled for the
/// actor it names signed it (the host's judgement of a lock ref, and the merge guard's).
#[must_use]
pub fn signed_lock(
    git: &crate::git::Git,
    registry: Option<&Registry>,
    commit: &str,
) -> Option<Lock> {
    let lock = git
        .file_at(commit, FILE)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<Lock>(&text).ok())
        .filter(|lock| lock.schema == SCHEMA)?;
    let Some(registry) = registry else {
        return Some(lock);
    };
    let file = registry
        .principal_file(git, KeyClass::Agent, &lock.actor)
        .ok()?;
    let signer = crate::signing::verify_commit(git, commit, &file).ok()??;
    (signer == lock.actor).then_some(lock)
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
    let registry = signing_registry(repo);
    // Under a registry, a commit no enrolled key of its actor signed is no holder's: the walk
    // stops at it, so a forged commit never names a change, nor hides the holder before it.
    let mut locks = history
        .lines()
        .map_while(|commit| lock_at(repo, registry.as_ref(), commit))
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
    let changed = repo
        .git
        .run(&["diff", "--name-only", "--no-renames", "-z", trunk, tree])
        .map_err(|error| error.message())?;
    let changed: Vec<&str> = changed
        .split('\0')
        .filter(|path| !path.is_empty())
        .collect();
    let lockable = lockable(&repo.git, trunk, &changed)?;
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
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{lock_ref, lockable};

    #[test]
    fn a_lock_ref_is_a_bay_ref_named_by_the_path() {
        let name = lock_ref("art/logo.psd");
        assert!(crate::names::bay_ref(&name).is_ok(), "{name}");
        assert_ne!(name, lock_ref("art/logo2.psd"));
    }

    #[test]
    fn the_attribute_at_the_commit_names_the_lockable_paths() {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(dir.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(["-c", "user.name=t", "-c", "user.email=t@bay.invalid"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        };
        run(&["init", "-q"]);
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.psd lockable\nmodels/** lockable\nmodels/readme.txt -lockable\n",
        )
        .unwrap();
        run(&["add", ".gitattributes"]);
        run(&["commit", "-q", "-m", "attributes"]);
        let git = crate::git::Git::new(dir.path());
        let paths = [
            "logo.psd",
            "models/big.bin",
            "models/readme.txt",
            "src/a.rs",
        ];
        assert_eq!(
            lockable(&git, "HEAD", &paths).unwrap(),
            ["logo.psd", "models/big.bin"]
        );
        assert_eq!(lockable(&git, "HEAD", &[]).unwrap(), Vec::<String>::new());
    }
}

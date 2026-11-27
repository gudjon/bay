//! A view: the sparse checkout of one change's declared inputs (`DESIGN` § 10).
//!
//! A view is a worktree of the repository at `<bay state dir>/views/<repository>/<id>` (G663, one
//! opened earlier stays at `<git-common-dir>/bay/views/<id>`), detached
//! at the change's draft. With declared paths it is sparse: an absent file is a
//! skip-worktree entry of the index, so a draft built from the index never reads it as a
//! deletion (§ 10, G27). The shared blob-less machine cache is kickstart item 15.
//!
//! G212: in that cache a view fetches only the blobs of its declared paths (and the hook-settings
//! file bay adds): only its checkout may fetch, after the sparse set is written. Every other
//! read of a view's Git runs without lazy fetch (G104b).
//!
//! G520 (B4): a view is its actor's. The journal records the actor that made it, or took it over
//! (`bay start <change> --adopt`, or the adoption of `bay start <intent>`), and any other actor's
//! write there is refused `VIEW_NOT_YOURS`: `bay merge`, `bay save` and `bay start <change>`
//! ([`not_yours`]), and the agent hook's edit or `git commit` ([`actor_at`]). The actor is the
//! name bay attributes work to (`crate::change::actor_name`). A view an older bay made records
//! none, and refuses no one.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::clock::now_rfc3339;
use crate::git::{Git, GitError};
use crate::repo::Repo;
use crate::result::{BayResult, Reason};
use crate::version::{GIT_BACKFILL_PATHSPEC, Version};

/// The view journal (§ 10): `creating → ready → checkpointed → published → releasable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Journal {
    Creating,
    Ready,
    Checkpointed,
    Published,
    Releasable,
}

impl Journal {
    /// Whether the view is complete. Only a complete view can be frozen into a draft.
    #[must_use]
    pub const fn is_complete(self) -> bool {
        !matches!(self, Self::Creating)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalFile {
    change: String,
    state: Journal,
    at: String,
    /// When the view was first created, kept across state changes.
    #[serde(default)]
    created_unix: u64,
    /// G520: the actor the view is for, kept across state changes; absent in a view an older
    /// bay made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    actor: Option<String>,
}

use crate::live::unix_now;

fn read_journal(repo: &Repo, id: &str) -> Option<JournalFile> {
    let text = std::fs::read_to_string(journal_path(repo, id)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Seconds since the view was created.
#[must_use]
pub fn age_s(repo: &Repo, id: &str) -> Option<u64> {
    read_journal(repo, id).map(|file| unix_now().saturating_sub(file.created_unix))
}

/// The change ids of every view on this machine, from their journals.
#[must_use]
pub fn all(repo: &Repo) -> Vec<String> {
    let mut ids: Vec<String> = repo
        .views_dirs()
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            name.strip_suffix(".journal").map(str::to_owned)
        })
        .collect();
    ids.sort();
    ids
}

/// Whether the view holds edits that no checkpoint records: its files differ from the
/// latest draft's tree, or it has untracked files. Absent sparse files are not edits.
///
/// # Errors
///
/// A git fault (a view that cannot be read is reported by the caller).
pub fn uncheckpointed(view: &Path, latest_draft: &str) -> Result<bool, GitError> {
    let git = Git::new(view);
    let tree = format!("{latest_draft}^{{tree}}");
    let (code, _, stderr) = git.run_status(&["diff", "--quiet", &tree])?;
    match code {
        0 => {}
        1 => return Ok(true),
        _ => {
            return Err(GitError::Failed {
                args: "diff --quiet".to_owned(),
                stderr,
            });
        }
    }
    Ok(!git
        .run(&["ls-files", "--others", "--exclude-standard"])?
        .is_empty())
}

/// G735: a stamp of what a view's check reads that a writer moves: its index file and `HEAD`
/// (a checkpoint, a commit or `git add` rewrites them by rename) and its root directory (a file
/// made or removed there); `None` for a view whose Git directory cannot be found.
#[must_use]
pub fn stamp(view: &Path) -> Option<String> {
    let link = std::fs::read_to_string(view.join(".git")).ok()?;
    let git_dir = view.join(link.trim().strip_prefix("gitdir:")?.trim());
    let mark = |path: &Path| -> String {
        std::fs::metadata(path).map_or_else(
            |_| "-".to_owned(),
            |meta| {
                let nanos = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |time| time.as_nanos());
                #[cfg(unix)]
                let inode = std::os::unix::fs::MetadataExt::ino(&meta);
                #[cfg(not(unix))]
                let inode = 0_u64;
                format!("{nanos}/{}/{inode}", meta.len())
            },
        )
    };
    Some(format!(
        "{} {} {}",
        mark(&git_dir.join("index")),
        mark(&git_dir.join("HEAD")),
        mark(view)
    ))
}

/// The journal sits beside the view, outside it: `<id>.journal`.
#[must_use]
pub fn journal_path(repo: &Repo, id: &str) -> PathBuf {
    repo.view_home(id).join(format!("{id}.journal"))
}

/// G824: the lock of change `id`'s view, beside its journal: `<id>.lock`. A release holds it
/// while it removes the view, and a merge while it reads the view, so neither reads the other's
/// work half done.
fn lock_path(repo: &Repo, id: &str) -> PathBuf {
    repo.view_home(id).join(format!("{id}.lock"))
}

fn open_lock(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
}

/// G824 (a fleet finding, 2026-11-20: a `bay merge --wait` read its merged change's view as
/// another session's `bay start` removed it, and answered that bay could not finish): holds the
/// lock of change `id`'s view while a merge reads it, once a release under way has ended. `None`
/// when the view has no journal, or the lock cannot be taken; the merge then reads as before.
#[must_use]
pub fn hold(repo: &Repo, id: &str) -> Option<std::fs::File> {
    if !journal_path(repo, id).exists() {
        return None;
    }
    let path = lock_path(repo, id);
    let file = open_lock(&path).ok()?;
    file.lock().ok()?;
    // A release ended while this waited: the view is gone, and its lock is no longer needed.
    if !journal_path(repo, id).exists() {
        let _ = std::fs::remove_file(&path);
    }
    Some(file)
}

/// The view's journal state. A view with no journal is treated as incomplete.
#[must_use]
pub fn journal(repo: &Repo, id: &str) -> Option<Journal> {
    let text = std::fs::read_to_string(journal_path(repo, id)).ok()?;
    serde_json::from_str::<JournalFile>(&text)
        .ok()
        .map(|file| file.state)
}

/// Records a journal state, atomically. The view keeps its actor.
///
/// # Errors
///
/// The journal cannot be written.
pub fn set_journal(repo: &Repo, id: &str, state: Journal) -> Result<(), String> {
    write_journal(repo, id, state, None)
}

/// G520: the actor the view of change `id` is for, from its journal; `None` when the view has
/// no journal, or one an older bay wrote.
#[must_use]
pub fn actor(repo: &Repo, id: &str) -> Option<String> {
    read_journal(repo, id).and_then(|file| file.actor)
}

/// G520: the refusal `VIEW_NOT_YOURS` of a write in the view of change `id` by this command's
/// actor, when the view is another actor's; `None` when it is this actor's, or records none (a
/// view an older bay made). Its detail names both actors, and the take-over.
#[must_use]
pub fn not_yours(repo: &Repo, id: &str) -> Option<BayResult> {
    refused_for(repo, id, &actor(repo, id)?)
}

/// G520: [`not_yours`] of the view of change `id` as `owner`'s, whether or not it records
/// that: `bay start <change>` refuses to make a view of another actor's change.
#[must_use]
pub fn refused_for(repo: &Repo, id: &str, owner: &str) -> Option<BayResult> {
    let me = crate::change::actor_name();
    if owner == me {
        return None;
    }
    Some(
        BayResult::blocked(Reason::ViewNotYours)
            .with_change(id)
            .with_detail(json!({
                "view": repo.view_path(id).display().to_string(),
                "view_actor": owner,
                "actor": me,
                "actor_source": crate::change::actor_source(),
                "recovery": format!("work in a view of your own, or take this one over from {owner} with bay start {id} --adopt"),
            })),
    )
}

/// G520: the change and the actor of the view that holds the resolved `path` (the view's top
/// level, or a path in it), from the journal beside the view: a few file-system calls and no
/// Git, so the agent hook pays almost nothing per edit. `None` when no view holds it, or its
/// journal records no actor.
#[must_use]
pub fn actor_at(path: &Path) -> Option<(String, String)> {
    let top = path
        .ancestors()
        .find(|dir| dir.join(".git").symlink_metadata().is_ok())?;
    let views = top.parent()?;
    // `<bay>/views/<id>` under a common directory, `<bay>/views/<repository>/<id>` under the state
    // directory; the journal then decides.
    let is_views = |dir: &Path| {
        dir.file_name().is_some_and(|name| name == "views")
            && dir
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "bay")
    };
    if !is_views(views) && !views.parent().is_some_and(is_views) {
        return None;
    }
    let id = top.file_name()?.to_str()?;
    let text = std::fs::read_to_string(views.join(format!("{id}.journal"))).ok()?;
    let file: JournalFile = serde_json::from_str(&text).ok()?;
    if file.change != id {
        return None;
    }
    Some((id.to_owned(), file.actor?))
}

/// G520 (`--adopt`): the view of change `id` is `actor`'s from now on. Returns the actor it was
/// for when that was another one; a view with no journal is left as it is.
///
/// # Errors
///
/// The journal cannot be written.
pub fn take_over(repo: &Repo, id: &str, actor: &str) -> Result<Option<String>, String> {
    let Some(file) = read_journal(repo, id) else {
        return Ok(None);
    };
    if file.actor.as_deref() == Some(actor) {
        return Ok(None);
    }
    write_journal(repo, id, file.state, Some(actor))?;
    Ok(file.actor)
}

/// Writes the journal of change `id` with `state`, and `actor`, else the actor it holds.
fn write_journal(repo: &Repo, id: &str, state: Journal, actor: Option<&str>) -> Result<(), String> {
    // A temporary file of this writer's own: two processes recording the same journal at once
    // (two `bay status` runs) each rename a whole file, and the later rename wins.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = journal_path(repo, id);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    let tmp = path.with_extension(format!(
        "journal.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let before = read_journal(repo, id);
    let created_unix = before
        .as_ref()
        .map_or_else(unix_now, |file| file.created_unix);
    let file = JournalFile {
        change: id.to_owned(),
        state,
        at: now_rfc3339(),
        created_unix,
        actor: actor
            .map(str::to_owned)
            .or_else(|| before.and_then(|file| file.actor)),
    };
    let text = serde_json::to_string(&file).map_err(|error| error.to_string())?;
    std::fs::write(&tmp, text).map_err(|error| format!("{}: {error}", tmp.display()))?;
    crate::durable::rename(&tmp, &path).map_err(|error| format!("{}: {error}", path.display()))
}

/// What [`release_ended`] knows of one ended change (landed, abandoned or `no_effect`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ended {
    /// The revision the change ended with ([`crate::status::last_revision`]).
    pub revision: String,
    /// When the change ended, in Unix seconds.
    pub at_unix: u64,
    /// The commits bay itself leaves a view's `HEAD` at: the change's drafts and bases. Any
    /// other `HEAD` but `revision` is a commit made in the view.
    pub placed: Vec<String>,
    /// The change's own retention window, in place of the one [`release_ended`] is given: a
    /// lane change's shorter one (NEW-4, `[[lane]] window_s`).
    pub window: Option<std::time::Duration>,
    /// G743: the head of the change's chain the rest was read at, which names it all.
    pub head: String,
}

/// G97: removes this machine's views of ended changes whose end is `window` (or the change's
/// own [`Ended::window`]) or more ago.
/// `ended` names each such change, and `None` for one that has not ended or whose view is in
/// use. Right before each removal it checks that the view holds nothing newer than the revision
/// the change ended with: its `HEAD` is the revision or a commit bay placed, its files equal the
/// revision's tree, and it has no untracked files. Files the view's ignore rules match are not
/// work and go with it. A directory that is not a worktree of this repository (no `.git`, or a
/// damaged one) and a locked worktree are kept, and so is a view whose worktree git does not
/// remove: nothing is deleted by hand. The view's worktree and its journal both go, so
/// `bay status` no longer lists it; a view whose directory is already gone loses its journal.
/// A view kept because it holds something newer still loses the files its ignore rules match
/// (G639).
///
/// One view's fault keeps that view and the pass goes on. Returns the ids it released.
#[must_use]
pub fn release_ended(
    repo: &Repo,
    window: std::time::Duration,
    ended: &dyn Fn(&str) -> Option<Ended>,
) -> Vec<String> {
    let now = unix_now();
    let mut released: Vec<String> = all(repo)
        .into_iter()
        .filter(|id| {
            ended(id).is_some_and(|change| {
                now.saturating_sub(change.at_unix) >= change.window.unwrap_or(window).as_secs()
                    && release(repo, id, &change).unwrap_or(false)
            })
        })
        .collect();
    released.extend(release_set_aside(repo, now, ended));
    released
}

/// G795: how long a folder in the views directory with no journal waits before a start removes
/// it: long enough that a view a start is making now, before its journal, is never one.
const ORPHAN_AGE_S: u64 = 24 * 3600;

/// G795 (a fleet finding, 2026-11-19: a folder held 6.4 GB of build output for six days in a
/// repository's views directory, where a build still running in a view had written it again
/// after the view's change ended and the view was removed; no journal named it and Git had no
/// worktree there, so nothing ever removed it). A start removes each folder in this repository's
/// views directories that no journal names, that is no worktree Git knows, and that nothing
/// touched for [`ORPHAN_AGE_S`]. Copies set aside as unsaved (`<id>.unsaved-…`) keep G784's
/// rule. Returns the folders removed.
pub fn sweep_orphans(repo: &Repo) -> Vec<String> {
    orphans(repo, true)
}

/// G795: the folders [`sweep_orphans`] removes; with `remove` false, only named (doctor's
/// storage row says what the next start frees).
pub fn orphans(repo: &Repo, remove: bool) -> Vec<String> {
    let registered: Vec<PathBuf> = repo
        .git
        .run(&["worktree", "list", "--porcelain"])
        .map(|out| {
            out.lines()
                .filter_map(|line| line.strip_prefix("worktree "))
                .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)))
                .collect()
        })
        .unwrap_or_default();
    let now = unix_now();
    let mut removed = Vec::new();
    for dir in repo.views_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_dir()
                || name.contains(".unsaved-")
                || dir.join(format!("{name}.journal")).exists()
            {
                continue;
            }
            let real = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if registered.iter().any(|known| known == &real) {
                continue;
            }
            let touched = meta
                .modified()
                .ok()
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(now, |at| at.as_secs());
            if now.saturating_sub(touched) < ORPHAN_AGE_S {
                continue;
            }
            if !remove || remove_tree(&path).is_ok() {
                removed.push(name);
            }
        }
    }
    removed
}

/// G784: the copies `bay start` set aside as unsaved (`<id>.unsaved-<unix seconds>`, see
/// [`discard`]) whose change has ended and which are a retention window old: each one's files,
/// those its ignore rules do not match, are saved in one commit of bay's own under the local ref
/// `refs/bay/view-kept/<id>.unsaved-<unix seconds>`, read back, and the copy goes. A copy whose
/// save cannot be verified stays. Returns the copies' names released.
fn release_set_aside(repo: &Repo, now: u64, ended: &dyn Fn(&str) -> Option<Ended>) -> Vec<String> {
    let window = repo.config.retention_window().as_secs();
    let mut released = Vec::new();
    for dir in repo.views_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().into_string().ok() else {
                continue;
            };
            let Some((id, at)) = name.split_once(".unsaved-") else {
                continue;
            };
            let Ok(at) = at.parse::<u64>() else {
                continue;
            };
            if now.saturating_sub(at) < window || ended(id).is_none() {
                continue;
            }
            let path = entry.path();
            if path.is_dir()
                && kept_loose_aside(repo, &name, &path)
                && saved_aside(repo, &name, &path).is_some()
                && remove_tree(&path).is_ok()
            {
                released.push(name);
            }
        }
    }
    released
}

/// G784: where the loose ignored files of the view or copy `name` are kept once it goes:
/// `<common>/bay/view-kept/<name>/`, a folder only this account reads.
fn loose_home(repo: &Repo, name: &str) -> PathBuf {
    repo.common_dir.join("bay").join("view-kept").join(name)
}

/// G784 (owner ruling, 2026-11-18, with G639's rule kept): copies each loose ignored file under
/// `root` (an ignored path that is a file, not in an ignored directory, such as an `.env`) to
/// `home`, at its own relative path and with its own mode, and reads each copy back byte for
/// byte. Such a file is no build output: it may hold a secret, so it is never written into a
/// Git object, and it outlives the view. Ignored directories are build output and are not kept.
/// `git` and `envs` name the work tree and its index. Whether every such file is kept; one that
/// cannot be copied, or a copy that differs, keeps the view.
fn kept_loose_ignored(git: &Git, envs: &[(&str, &str)], root: &Path, home: &Path) -> bool {
    let Ok(listed) = git.run_with(
        &[
            "--no-optional-locks",
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
        envs,
        None,
    ) else {
        return false;
    };
    let files: Vec<&str> = listed
        .split('\0')
        .filter(|entry| !entry.is_empty() && !entry.ends_with('/'))
        .collect();
    if files.is_empty() {
        return true;
    }
    if std::fs::create_dir_all(home).is_err() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700)).is_err() {
            return false;
        }
    }
    files.iter().all(|file| {
        let from = root.join(file);
        let to = home.join(file);
        if !from.is_file() {
            // A link, or anything else no copy can stand for, keeps the view.
            return false;
        }
        let Ok(bytes) = std::fs::read(&from) else {
            return false;
        };
        if let Ok(earlier) = std::fs::read(&to) {
            // A file kept before at the same path is never overwritten by another one.
            return earlier == bytes;
        }
        to.parent()
            .is_some_and(|parent| std::fs::create_dir_all(parent).is_ok())
            && std::fs::copy(&from, &to).is_ok()
            && std::fs::read(&to).is_ok_and(|copied| copied == bytes)
    })
}

/// [`kept_loose_ignored`] for the copy `name` set aside at `path`, read through an empty index
/// of bay's own.
fn kept_loose_aside(repo: &Repo, name: &str, path: &Path) -> bool {
    let git = Git::new(path);
    let common = repo.common_dir.display().to_string();
    let tree_dir = path.display().to_string();
    let index = repo
        .common_dir
        .join("bay")
        .join(format!("aside-loose-{}.index", std::process::id()));
    let _ = std::fs::remove_file(&index);
    let index_text = index.display().to_string();
    let envs = [
        ("GIT_DIR", common.as_str()),
        ("GIT_WORK_TREE", tree_dir.as_str()),
        ("GIT_INDEX_FILE", index_text.as_str()),
    ];
    let kept = git.run_with(&["read-tree", "--empty"], &envs, None).is_ok()
        && kept_loose_ignored(&git, &envs, path, &loose_home(repo, name));
    let _ = std::fs::remove_file(&index);
    kept
}

/// [`release_set_aside`]'s save of the copy `name` at `path`: its files through an index of
/// bay's own, in a commit with no parent but a commit kept under its ref before.
fn saved_aside(repo: &Repo, name: &str, path: &Path) -> Option<String> {
    let git = Git::new(path).writing_tree();
    let common = repo.common_dir.display().to_string();
    let tree_dir = path.display().to_string();
    let more = [
        ("GIT_DIR", common.as_str()),
        ("GIT_WORK_TREE", tree_dir.as_str()),
    ];
    let index = repo
        .common_dir
        .join("bay")
        .join(format!("aside-save-{}.index", std::process::id()));
    let _ = std::fs::remove_file(&index);
    let index_text = index.display().to_string();
    let mut empty = vec![("GIT_INDEX_FILE", index_text.as_str())];
    empty.extend_from_slice(&more);
    let saved = git
        .run_with(&["read-tree", "--empty"], &empty, None)
        .ok()
        .and_then(|_| save_tree(&git, &index, &more));
    let _ = std::fs::remove_file(&index);
    let tree = saved?;
    let kept = format!("{}{name}", crate::names::VIEW_KEPT);
    let earlier = repo.git.run(&["rev-parse", "--verify", "-q", &kept]).ok();
    let mut args = vec!["commit-tree", tree.as_str()];
    if let Some(earlier) = &earlier {
        args.extend(["-p", earlier.trim()]);
    }
    let message = format!("bay: the files set aside as {name}, saved before they were freed");
    args.extend(["-m", message.as_str()]);
    let commit = repo
        .git
        .run_with(&args, crate::change::RECORD_IDENTITY, None)
        .ok()?;
    let old = earlier.unwrap_or_default();
    repo.git
        .run(&["update-ref", &kept, commit.trim(), old.trim()])
        .ok()?;
    Some(commit)
}

/// G665: the views [`release_ended`] would release, removing nothing. `in_time` stops the reading
/// once it says no; then the second value is false.
pub fn would_release(
    repo: &Repo,
    window: std::time::Duration,
    ended: &dyn Fn(&str) -> Option<Ended>,
    in_time: &dyn Fn() -> bool,
) -> (Vec<String>, bool) {
    let now = unix_now();
    let mut free = Vec::new();
    for id in all(repo) {
        if !in_time() {
            return (free, false);
        }
        let due = ended(&id).filter(|change| {
            now.saturating_sub(change.at_unix) >= change.window.unwrap_or(window).as_secs()
        });
        if let Some(change) = due
            && matches!(
                standing(repo, &id, &change),
                Ok(Standing::Free | Standing::Gone)
            )
        {
            free.push(id);
        }
    }
    (free, true)
}

/// What cleanup may do with the view of ended change `id`.
enum Standing {
    /// Locked, or not a worktree of this repository: kept.
    Kept,
    /// It holds something newer than its change's end: kept, without its ignored directories.
    HoldsNewer,
    /// G743: judged holding something newer before, and nothing it holds moved since: kept as it
    /// is, judged and cleaned no more.
    HoldsNewerKept,
    /// Its directory is already gone: only its journal is left.
    Gone,
    /// It holds nothing newer, and may go.
    Free,
}

fn standing(repo: &Repo, id: &str, ended: &Ended) -> Result<Standing, GitError> {
    let path = repo.view_path(id);
    // Git names a worktree's administrative directory after the view's own directory.
    let lock = repo.common_dir.join("worktrees").join(id).join("locked");
    if lock.exists() {
        return Ok(Standing::Kept);
    }
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Standing::Gone),
        Err(error) => Err(GitError::Spawn(error)),
        Ok(_) => {
            if let Some(key) = holds_newer_key(&path, ended)
                && kept_until(repo, &key).is_some_and(|until| unix_now() < until)
            {
                return Ok(Standing::HoldsNewerKept);
            }
            let Some(admin) = worktree_admin(repo, &path) else {
                return Ok(Standing::Kept);
            };
            if admin.join("locked").exists() {
                return Ok(Standing::Kept);
            }
            if holds_nothing_newer(&path, ended)? {
                Ok(Standing::Free)
            } else {
                Ok(Standing::HoldsNewer)
            }
        }
    }
}

/// G743 (a measurement, 2026-11-15: on a shared store with 91 views of ended changes, each
/// agent's `bay start` judged every one again, about 1,200 Git calls and a walk of each view's
/// tree for ignored directories, 42 s at the median, though nobody touched those views): the key
/// a view's "holds something newer" is kept under: the view, the revision its change ended with
/// and the views it placed, and its stamp ([`stamp`]: its index, `HEAD` and root directory). A
/// view whose stamp moved, or whose change ended again, is judged again. `None` for a view
/// whose Git directory cannot be read.
fn holds_newer_key(path: &Path, ended: &Ended) -> Option<[u8; 32]> {
    holds_newer_at(path, &ended.head)
}

/// [`holds_newer_key`] of the view at `path` for the change whose chain head is `head`: the head
/// names the revision the change ended with and the commits it placed.
fn holds_newer_at(path: &Path, head: &str) -> Option<[u8; 32]> {
    let stamp = stamp(path)?;
    let view = path.display().to_string();
    // G769: a verdict cached before a view holding only bay's own commit could go is not read.
    // G784: nor one cached before a view's unsaved work could be saved and the view freed; a
    // verdict now holds the time until which it stands.
    Some(crate::facts::key(
        "view-release-4",
        "holds-newer",
        &[&view, head, &stamp],
        None,
    ))
}

/// G747: whether the view of change `id`, whose chain head is `head`, was judged holding
/// something newer and nothing in it moved since: a start then reads nothing more of it.
#[must_use]
pub fn kept_holding_newer(repo: &Repo, id: &str, head: &str) -> bool {
    holds_newer_at(&repo.view_path(id), head)
        .and_then(|key| kept_until(repo, &key))
        .is_some_and(|until| unix_now() < until)
}

/// G784: the Unix second until which a "holds something newer" verdict under `key` stands: the
/// time its view's unsaved work is due to be saved, or the next try after a save that failed.
fn kept_until(repo: &Repo, key: &[u8; 32]) -> Option<u64> {
    let kept = crate::facts::get(&repo.common_dir, key)?;
    std::str::from_utf8(&kept).ok()?.trim().parse().ok()
}

/// G784: how often a view whose unsaved work could not be saved is tried again.
const SAVE_RETRY_S: u64 = 86_400;

/// Releases the view of ended change `id` when it holds nothing newer; whether it did. G824: a
/// view whose lock a merge holds while it reads the view is left for the next start.
fn release(repo: &Repo, id: &str, ended: &Ended) -> Result<bool, GitError> {
    let lock_path = lock_path(repo, id);
    let lock = open_lock(&lock_path).map_err(GitError::Spawn)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
        Err(std::fs::TryLockError::Error(error)) => return Err(GitError::Spawn(error)),
    }
    let released = release_held(repo, id, ended);
    drop(lock);
    // A merge that waited on the lock finds the journal gone, and removes the lock if it is first.
    if matches!(released, Ok(true)) {
        let _ = std::fs::remove_file(&lock_path);
    }
    released
}

/// [`release`] under the view's lock.
fn release_held(repo: &Repo, id: &str, ended: &Ended) -> Result<bool, GitError> {
    let path = repo.view_path(id);
    match standing(repo, id, ended)? {
        Standing::Kept | Standing::HoldsNewerKept => return Ok(false),
        Standing::HoldsNewer => {
            // G639: the view stays for its work, and its change has ended, so the directories
            // its ignore rules match go. A loose ignored file, such as an `.env`, stays: bay
            // cannot tell it from work. One that fails is tried at the next start.
            remove_ignored_directories(&path);
            // G769: a view whose only "newer" thing is a commit, with nothing else in it, keeps
            // that commit under a ref of its own and goes.
            if committed_only(repo, id, &path) {
                remove_worktree(repo, &path).map_err(GitError::Spawn)?;
                unregister(repo, &path);
                return match std::fs::remove_file(journal_path(repo, id)) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        Err(GitError::Spawn(error))
                    }
                    _ => Ok(true),
                };
            }
            // G784 (owner ruling, 2026-11-18: "save then free"): once its change ended a
            // retention window ago, the view's unsaved work is saved to a local ref and the
            // view goes. A save that cannot be verified keeps the view, tried again a day on.
            let due = ended.at_unix.saturating_add(
                ended
                    .window
                    .unwrap_or_else(|| repo.config.retention_window())
                    .as_secs(),
            );
            let now = unix_now();
            let until = if now < due {
                due
            } else if kept_loose_ignored(&Git::new(&path), &[], &path, &loose_home(repo, id))
                && saved_work(repo, id, &path).is_some()
            {
                remove_worktree(repo, &path).map_err(GitError::Spawn)?;
                unregister(repo, &path);
                return match std::fs::remove_file(journal_path(repo, id)) {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        Err(GitError::Spawn(error))
                    }
                    _ => Ok(true),
                };
            } else {
                now + SAVE_RETRY_S
            };
            // G743: kept as judged, under the view's stamp after its cleaning, until `until`.
            if let Some(key) = holds_newer_key(&path, ended) {
                crate::facts::put(&repo.common_dir, &key, until.to_string().as_bytes());
            }
            return Ok(false);
        }
        Standing::Gone => {}
        Standing::Free => remove_worktree(repo, &path).map_err(GitError::Spawn)?,
    }
    unregister(repo, &path);
    match std::fs::remove_file(journal_path(repo, id)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(GitError::Spawn(error)),
        _ => Ok(true),
    }
}

/// G769 (owner, 2026-11-16: "greatly optimizing for disk space for users"; a Mac held 5.2 GB in
/// views of 15 merged changes, each clean at a commit its change's last draft does not name, so
/// each read as holding newer work): whether the view at `path` holds nothing but commits: no
/// edit, no untracked file and no ignored file at all (its ignored directories already went), its
/// `HEAD` is a commit bay wrote itself (an older draft; an agent's own commit keeps its view,
/// G97), and that `HEAD` is now kept by the local ref `refs/bay/view-kept/<id>`, so no commit of it is
/// lost when the view goes. Any doubt keeps the view.
fn committed_only(repo: &Repo, id: &str, path: &Path) -> bool {
    let git = Git::new(path);
    let clean = git
        .run(&[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--ignored",
            "--untracked-files=all",
        ])
        .is_ok_and(|out| out.trim().is_empty());
    if !clean {
        return false;
    }
    let Ok(head) = git.run(&["rev-parse", "--verify", "HEAD"]) else {
        return false;
    };
    // Only a commit bay wrote itself (an older draft of the change), never one an agent made:
    // a view with a commit of its own stays (G97).
    let bays = crate::change::RECORD_IDENTITY
        .iter()
        .find(|(key, _)| *key == "GIT_AUTHOR_EMAIL")
        .map(|(_, email)| *email);
    let author = git.run(&["log", "-1", "--format=%ae", "HEAD"]).ok();
    if author.as_deref().map(str::trim) != bays {
        return false;
    }
    let kept = format!("{}{id}", crate::names::VIEW_KEPT);
    repo.git.run(&["update-ref", &kept, head.trim()]).is_ok()
}

/// G784 (owner ruling, 2026-11-18: "save then free"; a Mac held about 4 GB in views of ended
/// changes kept only for their unsaved work): saves the work the view at `path` holds, its
/// tracked edits and its untracked files, in one commit of bay's own whose parent is the view's
/// `HEAD`, under the local ref `refs/bay/view-kept/<id>` (a ref kept there before stays a parent,
/// so no history of it is lost). Files the view's ignore rules match are not saved: they are
/// build output a later build makes again. The commit is built through an index of bay's own, a
/// copy of the view's, so the view's index is never written and a sparse view saves only what it
/// checks out. The save is read back before it counts: every file of the view must match the
/// saved index, and no file may be left out of it. The commit, or `None` when any step fails.
fn saved_work(repo: &Repo, id: &str, path: &Path) -> Option<String> {
    let git = Git::new(path).writing_tree();
    let own = git.run(&["rev-parse", "--git-path", "index"]).ok()?;
    let own = path.join(own.trim());
    let head = git.run(&["rev-parse", "--verify", "HEAD"]).ok()?;
    let kept = format!("{}{id}", crate::names::VIEW_KEPT);
    let index = repo
        .common_dir
        .join("bay")
        .join(format!("view-save-{}.index", std::process::id()));
    let saved = std::fs::copy(&own, &index)
        .ok()
        .and_then(|_| save_tree(&git, &index, &[]));
    let _ = std::fs::remove_file(&index);
    let tree = saved?;
    let earlier = repo.git.run(&["rev-parse", "--verify", "-q", &kept]).ok();
    let mut args = vec!["commit-tree", tree.as_str(), "-p", head.trim()];
    if let Some(earlier) = &earlier {
        args.extend(["-p", earlier.trim()]);
    }
    let message = format!("bay: the unsaved work of view {id}, saved before the view was freed");
    args.extend(["-m", message.as_str()]);
    let commit = repo
        .git
        .run_with(&args, crate::change::RECORD_IDENTITY, None)
        .ok()?;
    // Create-only, or move from exactly the ref read: a ref another process moved meanwhile
    // keeps this view.
    let old = earlier.unwrap_or_default();
    repo.git
        .run(&["update-ref", &kept, commit.trim(), old.trim()])
        .ok()?;
    Some(commit)
}

/// [`saved_work`]'s tree: every file of the view `git` reads, through the index `index`, and
/// whether that index then holds the view whole. `None` when a file differs from it or is left
/// out of it, or when Git fails.
fn save_tree(git: &Git, index: &Path, more: &[(&str, &str)]) -> Option<String> {
    let index_text = index.display().to_string();
    let mut envs = vec![("GIT_INDEX_FILE", index_text.as_str())];
    envs.extend_from_slice(more);
    let envs = envs.as_slice();
    git.run_with(&["add", "-A"], envs, None).ok()?;
    let (code, _, _) = git
        .run_status_with(&["--no-optional-locks", "diff-files", "--quiet"], envs)
        .ok()?;
    let left_out = git
        .run_with(&["ls-files", "--others", "--exclude-standard"], envs, None)
        .ok()?;
    if code != 0 || !left_out.trim().is_empty() {
        return None;
    }
    git.run_with(&["write-tree"], envs, None).ok()
}

/// G646: removes the worktree at `path`, which holds nothing to keep. A build tool may leave
/// read-only directories, which `git worktree remove` cannot empty, though it unregisters the
/// view all the same: bay then finishes the removal, or the view stays for good, unreported.
fn remove_worktree(repo: &Repo, path: &Path) -> std::io::Result<()> {
    // Plan W5: Git for Windows removes a path longer than 260 characters too.
    let path_text = path.display().to_string();
    let _ = repo
        .git
        .writing_tree()
        .run(&["worktree", "remove", "--force", &path_text]);
    if std::fs::symlink_metadata(path).is_ok() {
        remove_tree(path)?;
    }
    Ok(())
}

/// G647: drops the registration of the worktree at `path`, now gone, and no other's. `git worktree
/// prune` would also drop a sibling whose folder is briefly absent (an unmounted disk), which
/// then no longer works when its folder comes back.
fn unregister(repo: &Repo, path: &Path) {
    let path_text = path.display().to_string();
    let _ = repo.git.run(&["worktree", "remove", "--force", &path_text]);
}

/// G639: removes each directory the ignore rules of the view at `path` match, whole.
fn remove_ignored_directories(path: &Path) {
    let git = Git::new(path);
    let Ok(listed) = git.run(&[
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
    ]) else {
        return;
    };
    for directory in listed.lines().filter(|line| line.ends_with('/')) {
        let _ = remove_tree(&path.join(directory));
    }
}

/// G646: removes the tree at `path`, read-only directories too. Symbolic links are removed and
/// never followed, so nothing outside the tree changes.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    make_writable(path);
    std::fs::remove_dir_all(path)
}

/// Lets the owner empty every directory under `path`; one that cannot be read is left to the
/// removal's own error.
fn make_writable(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = meta.permissions().mode();
        if mode & 0o700 != 0o700 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o700));
        }
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        make_writable(&entry.path());
    }
}

/// The administrative directory (`<common-dir>/worktrees/<name>`) of the worktree whose top is
/// `path`, when it is a worktree of this repository; `None` for a directory with no `.git`
/// file, a damaged one, or another repository's.
fn worktree_admin(repo: &Repo, path: &Path) -> Option<PathBuf> {
    if !path.join(".git").is_file() {
        return None;
    }
    let listed = Git::new(path)
        .run(&[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ])
        .ok()?;
    let mut lines = listed.lines().map(|line| Path::new(line).canonicalize());
    let (Some(Ok(top)), Some(Ok(admin)), Some(Ok(common))) =
        (lines.next(), lines.next(), lines.next())
    else {
        return None;
    };
    let worktrees = common.join("worktrees");
    let ours = top == path.canonicalize().ok()?
        && common == repo.common_dir.canonicalize().ok()?
        && admin.parent() == Some(worktrees.as_path());
    ours.then_some(admin)
}

/// Whether the view at `path` holds nothing newer than the revision its change ended with.
fn holds_nothing_newer(path: &Path, ended: &Ended) -> Result<bool, GitError> {
    let git = Git::new(path);
    let head = git.run(&["rev-parse", "--verify", "HEAD"])?;
    // A fleet finding (2026-10-27): agents commit in their views, and the draft bay lands is its
    // own commit of that tree with that message, so a landed view kept a HEAD no draft names, and
    // one shared store kept 152 clean views of landed changes (15 GB, 450 worktrees). A HEAD with
    // the last revision's tree and message is what that revision copied; a commit with a message
    // of its own is still kept.
    let copied =
        |rev: &str| -> Result<String, GitError> { git.run(&["log", "-1", "--format=%T%n%B", rev]) };
    if head != ended.revision
        && !ended.placed.contains(&head)
        && copied(&head)? != copied(&ended.revision)?
    {
        return Ok(false);
    }
    Ok(!uncheckpointed(path, &ended.revision)?)
}

/// Removes the view of change `id` so it can be made again, never deleting work that no draft
/// holds: a view whose files differ from `saved` (the newest draft), has untracked files, or
/// cannot be compared at all is moved aside to `<views>/<id>.unsaved-<unix seconds>`, whole,
/// and that path is returned (a fleet finding, 2026-10-26: a view whose journal a full disk
/// left unfinished was removed by `bay start`, the repair its refusal names, and the next
/// submit was the draft before the agent's edits). Only a view that provably holds nothing
/// newer than `saved` is deleted.
///
/// # Errors
///
/// A git fault, or the view cannot be moved aside.
pub fn discard(repo: &Repo, id: &str, saved: &str) -> Result<Option<PathBuf>, GitError> {
    let path = repo.view_path(id);
    let mut kept = None;
    if path.exists() && holds_unsaved(repo, &path, saved) {
        let aside = repo
            .view_home(id)
            .join(format!("{id}.unsaved-{}", unix_now()));
        crate::durable::rename(&path, &aside).map_err(GitError::Spawn)?;
        kept = Some(aside);
    } else {
        remove_worktree(repo, &path).map_err(GitError::Spawn)?;
    }
    unregister(repo, &path);
    Ok(kept)
}

/// Whether the view at `path` holds a file that `saved` does not: one changed or added against
/// its tree, or untracked, ignored ones too (an `.env` is work no draft holds). A file the view
/// lacks is no work (a view made only in part lacks many), and a view that cannot be read is
/// taken to hold work.
///
/// G636 (a fleet finding, 2026-11-02: a full view's first checkout was cut off before git
/// wrote its index, so every file read as untracked, and `bay start` moved the whole view
/// aside as unsaved, 44,795 files): a view with no index is judged through an index of bay's
/// own, read from the view's commit, so only the files that differ from that commit, or that
/// it does not hold, count as work. The view's own files and index stay as they are.
fn holds_unsaved(repo: &Repo, path: &Path, saved: &str) -> bool {
    let git = Git::new(path);
    let Ok(index) = git.run(&["rev-parse", "--git-path", "index"]) else {
        return true;
    };
    let own = (!path.join(&index).exists()).then(|| {
        repo.common_dir
            .join("bay")
            .join(format!("view-{}.index", std::process::id()))
    });
    let own_text = own.as_ref().map(|own| own.display().to_string());
    let envs: Vec<(&str, &str)> = own_text
        .iter()
        .map(|own| ("GIT_INDEX_FILE", own.as_str()))
        .collect();
    let read = own.as_ref().map_or(Ok(String::new()), |own| {
        let _ = std::fs::remove_file(own);
        git.run_with(&["read-tree", "HEAD"], &envs, None)
    });
    let holds = read.is_err() || differs(&git, &envs, saved);
    if let Some(own) = &own {
        let _ = std::fs::remove_file(own);
    }
    holds
}

/// [`holds_unsaved`] once the view has an index to read, the one `envs` names or its own.
fn differs(git: &Git, envs: &[(&str, &str)], saved: &str) -> bool {
    let tree = format!("{saved}^{{tree}}");
    let changed = match git.run_status_with(&["diff", "--quiet", "--diff-filter=d", &tree], envs) {
        Ok((0, _, _)) => false,
        Ok(_) | Err(_) => true,
    };
    changed
        || git
            .run_with(
                &[
                    "ls-files",
                    "--others",
                    "--directory",
                    "--no-empty-directory",
                ],
                envs,
                None,
            )
            .map_or(true, |untracked| !untracked.is_empty())
}

/// A declared path as a non-cone sparse-checkout pattern rooted at the top: `src/` stays a
/// directory, `a.txt` a file. Git reads a non-cone pattern as a glob, so each character git's
/// own cone mode escapes (`\`, `*`, `?`, `[`) is escaped, and a trailing space, which git would
/// drop: `app/[org]/` names that one folder, never `app/o/` (BACKLOG 236). A leading `!` or `#`
/// cannot occur, since the pattern starts with `/`.
#[must_use]
pub fn sparse_pattern(path: &str) -> String {
    let trimmed = crate::inputs::plain(path);
    let kept = trimmed.trim_end_matches(' ');
    let mut pattern = String::with_capacity(trimmed.len() + 8);
    pattern.push('/');
    for character in kept.chars() {
        if matches!(character, '\\' | '*' | '?' | '[') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    for _ in kept.len()..trimmed.len() {
        pattern.push_str("\\ ");
    }
    pattern
}

/// The path a sparse pattern names, the inverse of [`sparse_pattern`]: `/app/\[org\]/` is
/// `app/[org]/`.
#[must_use]
pub fn pattern_path(pattern: &str) -> String {
    let mut path = String::with_capacity(pattern.len());
    let mut characters = pattern.trim_start_matches('/').chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => path.extend(characters.next()),
            other => path.push(other),
        }
    }
    path
}

/// Whether `list`, a view's `git sparse-checkout list`, holds `pattern`. Git prints a pattern
/// without a trailing space it would drop, and bay's git calls trim their output, so both
/// sides are compared trimmed.
fn lists(list: &str, pattern: &str) -> bool {
    list.lines().any(|line| line.trim() == pattern.trim())
}

/// Beside this repository's views under the state directory, the repository they belong to, so
/// `bay doctor` can say when a deleted clone left them behind (G663).
fn name_owner(repo: &Repo) -> std::io::Result<()> {
    let root = repo.views_dir();
    if root == repo.inner_views_dir() {
        return Ok(());
    }
    std::fs::write(
        root.with_extension("repository"),
        repo.common_dir.as_os_str().as_encoded_bytes(),
    )
}

/// The directories of views whose repository is gone: a deleted or moved clone's. Nothing can
/// tell whether they hold unsaved work with their repository gone, so they are only named.
#[must_use]
pub fn left_behind() -> Vec<PathBuf> {
    let Some(views) = crate::update::state_dir().map(|state| state.join("views")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(views) else {
        return Vec::new();
    };
    let mut left: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "repository"))
        .filter(|path| std::fs::read_to_string(path).is_ok_and(|owner| !Path::new(&owner).exists()))
        .map(|path| path.with_extension(""))
        .filter(|dir| dir.is_dir())
        .collect();
    left.sort();
    left
}

/// Creates the view of change `id` at `commit`, sparse over `paths` when any are declared, for
/// this command's actor (G520).
///
/// # Errors
///
/// A git fault. A view that already exists is reported by git.
pub fn create(repo: &Repo, id: &str, commit: &str, paths: &[String]) -> Result<PathBuf, GitError> {
    let path = repo.view_path(id);
    std::fs::create_dir_all(repo.views_dir()).map_err(GitError::Spawn)?;
    name_owner(repo).map_err(GitError::Spawn)?;
    let path_text = path.display().to_string();
    let journal_error = |why| GitError::Failed {
        args: "view journal".to_owned(),
        stderr: why,
    };
    let actor = crate::change::actor_name();
    write_journal(repo, id, Journal::Creating, Some(&actor)).map_err(journal_error)?;
    repo.git.run(&[
        "worktree",
        "add",
        "-q",
        "--no-checkout",
        "--detach",
        &path_text,
        commit,
    ])?;
    if std::env::var("BAY_TEST_KILL_AT").is_ok_and(|at| at == "start-mid-creation") {
        std::process::exit(137);
    }
    // G212: the only calls of a view's creation that may fetch, and only the declared files
    // they write: the sparse set comes first, so the checkout writes (and fetches) no other blob.
    // Plan W5: the checkout writes each blob's bytes, and long paths on Windows.
    let view = Git::new(&path).fetching().writing_tree();
    if !paths.is_empty() {
        let mut args = vec![
            "sparse-checkout".to_owned(),
            "set".to_owned(),
            "--no-cone".to_owned(),
        ];
        args.extend(paths.iter().map(|path| sparse_pattern(path)));
        args.extend(hook_settings(repo, commit).map(sparse_pattern));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        view.run(&args)?;
    }
    view.run(&["checkout", "-q", "--detach", commit])?;
    // BACKLOG 49 phase 2: the declared files the manifest keeps outside Git.
    let covered = |file: &str| {
        paths.is_empty()
            || paths
                .iter()
                .any(|declared| crate::inputs::within(declared, file))
    };
    crate::assets::fill_tree(repo, &path, commit, &covered).map_err(|why| GitError::Failed {
        args: "assets".to_owned(),
        stderr: why,
    })?;
    set_journal(repo, id, Journal::Ready).map_err(journal_error)?;
    Ok(path)
}

/// The agent product's hook-settings file when the adapter names one and `commit` holds it
/// (H7, G72). A sparse view holds it beside the declared paths: the view is its own Git top
/// level, so without it a session started there runs without bay's project hooks.
fn hook_settings(repo: &Repo, commit: &str) -> Option<&'static str> {
    let file = crate::agent_settings::hook_settings_file(&repo.config)?;
    // The tree names the file; its blob need not be in a blob-less store yet (G104b).
    repo.git
        .run(&["rev-parse", "--verify", "-q", &format!("{commit}:{file}")])
        .ok()
        .map(|_| file)
}

/// Adds to the sparse view of change `id` each of the `declared` paths its sparse set lacks, and
/// writes their files and the declared files the manifest keeps outside Git at the view's commit
/// (BACKLOG 143): a change that declares more paths holds them in the view it has, and a pick-up
/// after a refused one adds them still. A full view already holds every path.
///
/// # Errors
///
/// A git fault.
pub fn keep_declared(repo: &Repo, id: &str, declared: &[String]) -> Result<(), GitError> {
    if declared.is_empty() {
        return Ok(());
    }
    let path = repo.view_path(id);
    // As at the view's creation (G212): the only calls that fetch, and only the added files.
    let view = Git::new(&path).fetching().writing_tree();
    let list = view.run(&["sparse-checkout", "list"])?;
    let missing: Vec<&String> = declared
        .iter()
        .filter(|declared| !lists(&list, &sparse_pattern(declared)))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut args = vec!["sparse-checkout".to_owned(), "add".to_owned()];
    args.extend(missing.iter().map(|declared| sparse_pattern(declared)));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    view.run(&args)?;
    let head = view.run(&["rev-parse", "HEAD"])?;
    let covered = |file: &str| {
        missing
            .iter()
            .any(|added| crate::inputs::within(added, file))
    };
    crate::assets::fill_tree(repo, &path, &head, &covered).map_err(|why| GitError::Failed {
        args: "assets".to_owned(),
        stderr: why,
    })?;
    Ok(())
}

/// Puts the hook-settings file back into the sparse view of change `id` when its pattern is
/// missing and the view's commit holds the file: `--resume` keeps it in the view, whether the
/// view predates it or its sparse set was changed by hand. A full view already holds it.
///
/// # Errors
///
/// A git fault.
pub fn keep_hook_settings(repo: &Repo, id: &str, declared: &[String]) -> Result<(), GitError> {
    if declared.is_empty() {
        return Ok(());
    }
    let view = Git::new(repo.view_path(id));
    let head = view.run(&["rev-parse", "HEAD"])?;
    let Some(file) = hook_settings(repo, &head) else {
        return Ok(());
    };
    let pattern = sparse_pattern(file);
    let list = view.run(&["sparse-checkout", "list"])?;
    if lists(&list, &pattern) {
        return Ok(());
    }
    view.fetching()
        .writing_tree()
        .run(&["sparse-checkout", "add", &pattern])
        .map(|_| ())
}

/// How long the history warm-up may take: it fetches every past version of the declared paths,
/// which can be far more than the one tip a view's checkout fetches.
const WARM_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(10);

/// A declared path as the pathspec the history warm-up names: `src/` is `src`. `git backfill`
/// matches no directory written with its trailing `/`, and exits 0 having fetched nothing.
#[must_use]
pub fn history_pathspec(path: &str) -> String {
    let trimmed = crate::inputs::plain(path).trim_end_matches('/');
    if trimmed.is_empty() {
        ".".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The objects of the history of `pathspec` at the view's `HEAD` that its store does not hold:
/// the `?` lines of a missing-object walk, which itself never fetches.
fn missing_history(view: &Git, pathspec: &[String]) -> Result<Vec<String>, GitError> {
    let mut args = vec!["rev-list", "--objects", "--missing=print", "HEAD", "--"];
    args.extend(pathspec.iter().map(String::as_str));
    let listed = view.run_with(&args, &[("GIT_NO_LAZY_FETCH", "1")], None)?;
    Ok(listed
        .lines()
        .filter_map(|line| line.strip_prefix('?'))
        .map(str::to_owned)
        .collect())
}

/// The bytes of the objects `ids` that the store holds; an absent one counts nothing.
fn held_bytes(view: &Git, ids: &[String]) -> Result<u64, GitError> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut input = ids.join("\n");
    input.push('\n');
    let sizes = view.run_with(
        &["cat-file", "--batch-check=%(objectsize)"],
        &[("GIT_NO_LAZY_FETCH", "1")],
        Some(input.as_bytes()),
    )?;
    Ok(sizes
        .lines()
        .filter_map(|line| line.trim().parse::<u64>().ok())
        .sum())
}

/// G175: warms the history of the declared `paths` in the view at `view` (`[view]
/// warm_history`), and says how much it fetched. `git backfill --no-sparse -- <paths>` fetches
/// every past version of the declared paths in batches, so `git log -p` and `git blame` on them
/// read nothing lazily, offline too. It needs the pathspec form of Git 2.54; below that the
/// answer is `skipped` with the floor. A view with no declared paths is skipped: its history is
/// the whole repository's.
///
/// Backfill's exit status proves nothing (`--sparse` in a non-cone view, or a pathspec ending in
/// `/`, fetch nothing and exit 0), so the answer is the missing-object count over the declared
/// paths before and after: `warmed` only when none is left. A warm-up is optional: a fault is
/// reported as `failed`, never as the start's.
#[must_use]
pub fn warm_history(view: &Path, paths: &[String]) -> serde_json::Value {
    let git = Git::new(view);
    let version = git
        .version()
        .map_or_else(|_| Version::parse("0"), |line| Version::parse(&line));
    if version < Version::parse(GIT_BACKFILL_PATHSPEC) {
        return json!({"state": "skipped", "git": version.to_string(),
            "floor": GIT_BACKFILL_PATHSPEC});
    }
    if paths.is_empty() {
        return json!({"state": "skipped", "why": "no declared paths"});
    }
    let pathspec: Vec<String> = paths.iter().map(|path| history_pathspec(path)).collect();
    let failed =
        |error: &GitError| json!({"state": "failed", "paths": pathspec, "error": error.message()});
    let before = match missing_history(&git, &pathspec) {
        Ok(before) => before,
        Err(error) => return failed(&error),
    };
    if !before.is_empty() {
        // The intended fetch: a store whose reads never fetch lazily still lets this one fetch.
        let mut cmd = git.command();
        cmd.env_remove("GIT_NO_LAZY_FETCH")
            .args(["backfill", "--no-sparse", "--"])
            .args(&pathspec);
        let ran = crate::git::run_bounded_for(cmd, "backfill", None, WARM_TIMEOUT);
        match ran {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                return failed(&GitError::Failed {
                    args: "backfill".to_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                });
            }
            Err(error) => return failed(&error),
        }
    }
    let after = match missing_history(&git, &pathspec) {
        Ok(after) => after,
        Err(error) => return failed(&error),
    };
    let left: std::collections::HashSet<&str> = after.iter().map(String::as_str).collect();
    let fetched: Vec<String> = before
        .iter()
        .filter(|id| !left.contains(id.as_str()))
        .cloned()
        .collect();
    let bytes = match held_bytes(&git, &fetched) {
        Ok(bytes) => bytes,
        Err(error) => return failed(&error),
    };
    json!({
        "state": if after.is_empty() { "warmed" } else { "partial" },
        "paths": pathspec,
        "missing_before": before.len(),
        "missing_after": after.len(),
        "fetched": fetched.len(),
        "bytes": bytes,
    })
}

/// Whether the view at `path` exists as a worktree directory.
#[must_use]
pub fn exists(path: &Path) -> bool {
    path.join(".git").exists()
}

/// The paths a sparse view holds beyond `declared`: patterns added after `start` (for example
/// with `git sparse-checkout add`). A full view has none. `bay_file`, the hook-settings file
/// bay adds, is never an input the change read.
///
/// # Errors
///
/// A git fault.
pub fn undeclared_paths(
    view: &Path,
    declared: &[String],
    bay_file: Option<&str>,
) -> Result<Vec<String>, GitError> {
    let git = Git::new(view);
    let (code, list, _) = git.run_status(&["sparse-checkout", "list"])?;
    if code != 0 {
        return Ok(Vec::new());
    }
    let declared: Vec<String> = declared
        .iter()
        .map(String::as_str)
        .chain(bay_file)
        .map(sparse_pattern)
        .collect();
    Ok(list
        .lines()
        .map(str::trim)
        .filter(|pattern| {
            !pattern.is_empty() && !declared.iter().any(|known| known.trim() == *pattern)
        })
        .map(pattern_path)
        .collect())
}

/// Adds the view's undeclared paths to `observed`, keeping order and dropping repeats.
/// Returns whether anything was added.
///
/// # Errors
///
/// A git fault.
pub fn record_observed(
    repo: &Repo,
    view: &Path,
    declared: &[String],
    observed: &mut Vec<String>,
) -> Result<bool, GitError> {
    let mut added = false;
    let bay_file = crate::agent_settings::hook_settings_file(&repo.config);
    for path in undeclared_paths(view, declared, bay_file)? {
        if !observed.contains(&path) {
            observed.push(path);
            added = true;
        }
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::{Journal, journal, lists, pattern_path, set_journal, sparse_pattern};
    use crate::repo::Repo;

    #[test]
    fn a_sparse_pattern_escapes_what_git_reads_as_a_glob() {
        // BACKLOG 236: a Next.js dynamic-route folder is a literal path, never a class.
        // `]` closes no class once its `[` is escaped, and git's cone mode leaves it bare.
        assert_eq!(
            sparse_pattern("app/[org]/[venue]/cal/"),
            r"/app/\[org]/\[venue]/cal/"
        );
        assert_eq!(sparse_pattern(r"./a*b?c\d"), r"/a\*b\?c\\d");
        assert_eq!(sparse_pattern("/src/"), "/src/");
        assert_eq!(sparse_pattern("name  "), r"/name\ \ ");
        assert_eq!(sparse_pattern("a b/c"), "/a b/c");
        assert_eq!(sparse_pattern("#x"), "/#x");
        assert_eq!(sparse_pattern("!x"), "/!x");
    }

    #[test]
    fn a_sparse_pattern_names_the_path_it_was_made_from() {
        for path in [
            "src/",
            "app/[org]/[venue]/cal/",
            r"a*b?c\d",
            "name  ",
            "a b/c",
            "#x",
        ] {
            assert_eq!(pattern_path(&sparse_pattern(path)), path, "{path}");
        }
        // A pattern written by hand with git's own escapes names the same path.
        assert_eq!(
            pattern_path(r"/app/\[org\]/\[venue\]/cal/"),
            "app/[org]/[venue]/cal/"
        );
    }

    #[test]
    fn a_listed_pattern_is_found_whether_or_not_its_trailing_space_was_trimmed() {
        let pattern = sparse_pattern("name ");
        assert!(lists("/src/\n/name\\ ", &pattern));
        assert!(lists("/src/\n/name\\", &pattern));
        assert!(!lists("/src/\n/name", &pattern));
        assert!(lists(r"/app/\[org]/", &sparse_pattern("app/[org]/")));
        assert!(!lists("/app/[org]/", &sparse_pattern("app/[org]/")));
    }

    #[test]
    fn concurrent_journal_writes_never_fail() {
        // Two `bay status` runs record the same view's journal at once; each writes a whole
        // file of its own and renames it into place.
        let dir = tempfile::TempDir::new().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(init.success());
        // bay reads its config at the trunk tracking ref (ruling: config location).
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["-c", "user.name=t", "-c", "user.email=t@t.invalid"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        std::fs::create_dir_all(dir.path().join(".bay")).unwrap();
        std::fs::write(dir.path().join(".bay/config.toml"), "main = \"main\"\n").unwrap();
        git(&["add", ".bay/config.toml"]);
        git(&["commit", "-q", "-m", "config"]);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&["config", "bay.main", "main"]);
        let repo = Repo::open(dir.path()).unwrap();
        std::thread::scope(|scope| {
            let writers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        (0..100)
                            .map(|_| set_journal(&repo, "abcd2345", Journal::Releasable))
                            .collect::<Result<Vec<()>, String>>()
                    })
                })
                .collect();
            for writer in writers {
                writer.join().unwrap().unwrap();
            }
        });
        assert_eq!(journal(&repo, "abcd2345"), Some(Journal::Releasable));
    }
}

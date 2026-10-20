//! A view: the sparse checkout of one change's declared inputs (`DESIGN` § 10).
//!
//! A view is a worktree of the repository at `<git-common-dir>/bay/views/<id>`, detached
//! at the change's draft. With declared paths it is sparse: an absent file is a
//! skip-worktree entry of the index, so a draft built from the index never reads it as a
//! deletion (§ 10, G27). The shared blob-less machine cache is kickstart item 15.
//!
//! G212: in that cache a view fetches only the blobs of its declared paths (and the hook-settings
//! file bay adds): only its checkout may fetch, after the sparse set is written. Every other
//! read of a view's Git runs without lazy fetch (G104b).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::clock::now_rfc3339;
use crate::git::{Git, GitError};
use crate::repo::Repo;

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
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

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
    let Ok(entries) = std::fs::read_dir(repo.views_dir()) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
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

/// The journal sits beside the view, outside it: `<git-common-dir>/bay/views/<id>.journal`.
#[must_use]
pub fn journal_path(repo: &Repo, id: &str) -> PathBuf {
    repo.views_dir().join(format!("{id}.journal"))
}

/// The view's journal state. A view with no journal is treated as incomplete.
#[must_use]
pub fn journal(repo: &Repo, id: &str) -> Option<Journal> {
    let text = std::fs::read_to_string(journal_path(repo, id)).ok()?;
    serde_json::from_str::<JournalFile>(&text)
        .ok()
        .map(|file| file.state)
}

/// Records a journal state, atomically.
///
/// # Errors
///
/// The journal cannot be written.
pub fn set_journal(repo: &Repo, id: &str, state: Journal) -> Result<(), String> {
    // A temporary file of this writer's own: two processes recording the same journal at once
    // (two `bay status` runs) each rename a whole file, and the later rename wins.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::fs::create_dir_all(repo.views_dir()).map_err(|error| error.to_string())?;
    let path = journal_path(repo, id);
    let tmp = path.with_extension(format!(
        "journal.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let created_unix = read_journal(repo, id).map_or_else(unix_now, |file| file.created_unix);
    let file = JournalFile {
        change: id.to_owned(),
        state,
        at: now_rfc3339(),
        created_unix,
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
}

/// G97: removes this machine's views of ended changes whose end is `window` or more ago.
/// `ended` names each such change, and `None` for one that has not ended or whose view is in
/// use. Right before each removal it checks that the view holds nothing newer than the revision
/// the change ended with: its `HEAD` is the revision or a commit bay placed, its files equal the
/// revision's tree, and it has no untracked files. Files the view's ignore rules match are not
/// work and go with it. A directory that is not a worktree of this repository (no `.git`, or a
/// damaged one) and a locked worktree are kept, and so is a view whose worktree git does not
/// remove: nothing is deleted by hand. The view's worktree and its journal both go, so
/// `bay status` no longer lists it; a view whose directory is already gone loses its journal.
///
/// One view's fault keeps that view and the pass goes on. Returns the ids it released.
#[must_use]
pub fn release_ended(
    repo: &Repo,
    window: std::time::Duration,
    ended: &dyn Fn(&str) -> Option<Ended>,
) -> Vec<String> {
    let now = unix_now();
    all(repo)
        .into_iter()
        .filter(|id| {
            ended(id).is_some_and(|change| {
                now.saturating_sub(change.at_unix) >= window.as_secs()
                    && release(repo, id, &change).unwrap_or(false)
            })
        })
        .collect()
}

/// Releases the view of ended change `id` when it holds nothing newer; whether it did.
fn release(repo: &Repo, id: &str, ended: &Ended) -> Result<bool, GitError> {
    let path = repo.view_path(id);
    // Git names a worktree's administrative directory after the view's own directory.
    let lock = repo.common_dir.join("worktrees").join(id).join("locked");
    if lock.exists() {
        return Ok(false);
    }
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(GitError::Spawn(error)),
        Ok(_) => {
            let Some(admin) = worktree_admin(repo, &path) else {
                return Ok(false);
            };
            if admin.join("locked").exists() || !holds_nothing_newer(&path, ended)? {
                return Ok(false);
            }
            let path_text = path.display().to_string();
            repo.git
                .run(&["worktree", "remove", "--force", &path_text])?;
        }
    }
    let _ = repo.git.run(&["worktree", "prune"]);
    match std::fs::remove_file(journal_path(repo, id)) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(GitError::Spawn(error)),
        _ => Ok(true),
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
    let head = Git::new(path).run(&["rev-parse", "--verify", "HEAD"])?;
    if head != ended.revision && !ended.placed.contains(&head) {
        return Ok(false);
    }
    Ok(!uncheckpointed(path, &ended.revision)?)
}

/// Removes a half-made view so it can be created again.
///
/// # Errors
///
/// A git fault.
pub fn discard(repo: &Repo, id: &str) -> Result<(), GitError> {
    let path = repo.view_path(id);
    let path_text = path.display().to_string();
    let _ = repo.git.run(&["worktree", "remove", "--force", &path_text]);
    if path.exists() {
        std::fs::remove_dir_all(&path).map_err(GitError::Spawn)?;
    }
    repo.git.run(&["worktree", "prune"]).map(|_| ())
}

/// A declared path as a non-cone sparse-checkout pattern rooted at the top: `src/` stays a
/// directory, `a.txt` a file.
#[must_use]
pub fn sparse_pattern(path: &str) -> String {
    let trimmed = path.trim_start_matches("./").trim_start_matches('/');
    format!("/{trimmed}")
}

/// Creates the view of change `id` at `commit`, sparse over `paths` when any are declared.
///
/// # Errors
///
/// A git fault. A view that already exists is reported by git.
pub fn create(repo: &Repo, id: &str, commit: &str, paths: &[String]) -> Result<PathBuf, GitError> {
    let path = repo.view_path(id);
    std::fs::create_dir_all(repo.views_dir()).map_err(GitError::Spawn)?;
    let path_text = path.display().to_string();
    let journal_error = |why| GitError::Failed {
        args: "view journal".to_owned(),
        stderr: why,
    };
    set_journal(repo, id, Journal::Creating).map_err(journal_error)?;
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
    let view = Git::new(&path).fetching();
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
    if list.lines().any(|line| line.trim() == pattern) {
        return Ok(());
    }
    view.fetching()
        .run(&["sparse-checkout", "add", &pattern])
        .map(|_| ())
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
        .filter(|pattern| !pattern.is_empty() && !declared.iter().any(|known| known == pattern))
        .map(|pattern| pattern.trim_start_matches('/').to_owned())
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
    use super::{Journal, journal, set_journal};
    use crate::repo::Repo;

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
        std::fs::write(dir.path().join(".bay/config.toml"), "trunk = \"main\"\n").unwrap();
        git(&["add", ".bay/config.toml"]);
        git(&["commit", "-q", "-m", "config"]);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&["config", "bay.trunk", "main"]);
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

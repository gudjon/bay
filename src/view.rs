//! A view: the sparse checkout of one change's declared inputs (`DESIGN` § 10).
//!
//! A view is a worktree of the repository at `<git-common-dir>/bay/views/<id>`, detached
//! at the change's draft. With declared paths it is sparse: an absent file is a
//! skip-worktree entry of the index, so a draft built from the index never reads it as a
//! deletion (§ 10, G27). The shared blob-less machine cache is kickstart item 15.

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
    std::fs::create_dir_all(repo.views_dir()).map_err(|error| error.to_string())?;
    let path = journal_path(repo, id);
    let tmp = path.with_extension("journal.tmp");
    let created_unix = read_journal(repo, id).map_or_else(unix_now, |file| file.created_unix);
    let file = JournalFile {
        change: id.to_owned(),
        state,
        at: now_rfc3339(),
        created_unix,
    };
    let text = serde_json::to_string(&file).map_err(|error| error.to_string())?;
    std::fs::write(&tmp, text).map_err(|error| format!("{}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|error| format!("{}: {error}", path.display()))
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
    let view = Git::new(&path);
    if !paths.is_empty() {
        let mut args = vec![
            "sparse-checkout".to_owned(),
            "set".to_owned(),
            "--no-cone".to_owned(),
        ];
        args.extend(paths.iter().map(|path| sparse_pattern(path)));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        view.run(&args)?;
    }
    view.run(&["checkout", "-q", "--detach", commit])?;
    set_journal(repo, id, Journal::Ready).map_err(journal_error)?;
    Ok(path)
}

/// Whether the view at `path` exists as a worktree directory.
#[must_use]
pub fn exists(path: &Path) -> bool {
    path.join(".git").exists()
}

/// The paths a sparse view holds beyond `declared`: patterns added after `start` (for example
/// with `git sparse-checkout add`). A full view has none.
///
/// # Errors
///
/// A git fault.
pub fn undeclared_paths(view: &Path, declared: &[String]) -> Result<Vec<String>, GitError> {
    let git = Git::new(view);
    let (code, list, _) = git.run_status(&["sparse-checkout", "list"])?;
    if code != 0 {
        return Ok(Vec::new());
    }
    let declared: Vec<String> = declared.iter().map(|path| sparse_pattern(path)).collect();
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
    view: &Path,
    declared: &[String],
    observed: &mut Vec<String>,
) -> Result<bool, GitError> {
    let mut added = false;
    for path in undeclared_paths(view, declared)? {
        if !observed.contains(&path) {
            observed.push(path);
            added = true;
        }
    }
    Ok(added)
}

//! Claims within one tree (`DESIGN` § 11c, "Two sessions in one tree"; G101).
//!
//! Every change has its own isolated view, so a write in one view never touches another's
//! bytes. Two live sessions can still run in one tree: the main checkout, or a second session
//! opened inside a view. There a claim protects the uncommitted bytes. A claim is a path (relative
//! to the tree's top level, `/`-separated), a session id and a deadline (Unix seconds). A tree's
//! claims live in one file under the tree's own Git directory (`<git-dir>/bay/claims.json`, so a
//! linked worktree's view has its own), replaced by atomic rename.
//!
//! A session claims each path its step wrote (`PostToolBatch`, from the step's edit-tool targets
//! and, in a view, the files the step wrote), unless another live session of the tree holds it,
//! and each step moves the deadline of all its claims in that tree to the takeover window from
//! now. The session's own stop (not a refused one, ruling R7) and its end release them. A
//! subagent keeps its parent's session id, so it works under its parent's claims and claims for
//! its parent.
//!
//! `pre-tool` judges a write by reading the claims file of the target's tree: a few file-system
//! calls, no Git and no fetch. A claim never refuses a write in another tree: the file read is the
//! target's tree's, and its paths are relative to that tree. A claims file that cannot be read
//! holds nothing, but never silently: claims are a best-effort guard against two writers of one
//! file, and a corrupt file must not stop every write in the tree, so the write is allowed with
//! a `COVERAGE_UNKNOWN` notice that the session's row records (ruling R5). The next claim
//! replaces the file.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "bay.claims/1";
const FILE: &str = "claims.json";
const LOCK: &str = "claims.lock";
/// The largest claims file read.
const MAX_BYTES: u64 = 1024 * 1024;
/// The most words of one shell command judged.
const MAX_WORDS: usize = 256;
/// The file in a session's state directory that lists the claims files it wrote.
const STATE_FILE: &str = "claims";

/// One claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub path: String,
    pub session: String,
    pub deadline_s: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Claims {
    schema: String,
    claims: Vec<Claim>,
}

/// A live claim of another session on the path a write names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    /// The path in its tree.
    pub path: String,
    /// The holding session.
    pub holder: String,
    pub deadline_s: u64,
}

/// A work tree: its top level and its own Git directory.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Tree {
    root: PathBuf,
    git_dir: PathBuf,
}

impl Tree {
    fn file(&self) -> PathBuf {
        self.git_dir.join("bay").join(FILE)
    }
}

/// The tree that holds the resolved path `target`: the nearest ancestor with a `.git` entry, a
/// directory (the main checkout) or a `gitdir:` file (a linked worktree, such as a view).
fn tree_of(target: &Path) -> Option<Tree> {
    target.ancestors().skip(1).find_map(|dir| {
        let dot = dir.join(".git");
        if dot.is_dir() {
            return Some(Tree {
                root: dir.to_path_buf(),
                git_dir: dot,
            });
        }
        let mut text = String::new();
        std::fs::File::open(&dot)
            .ok()?
            .take(4096)
            .read_to_string(&mut text)
            .ok()?;
        let git_dir = text.strip_prefix("gitdir:")?.trim();
        Some(Tree {
            root: dir.to_path_buf(),
            git_dir: dir.join(git_dir),
        })
    })
}

/// `target`'s path in `tree`, `/`-separated; `None` for the top level itself or a path that
/// does not name a file by plain components.
fn relative(tree: &Tree, target: &Path) -> Option<String> {
    let inner = target.strip_prefix(&tree.root).ok()?;
    let parts: Vec<&str> = inner
        .components()
        .map(|part| match part {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The claims `file` holds; none when it is missing, too large or not a claims file. A writer
/// reads this way: what it cannot read, it replaces.
fn read(file: &Path) -> Vec<Claim> {
    judged(file).unwrap_or_default()
}

/// The claims `file` holds for a judgement: none when it is missing, else why it cannot be read.
fn judged(file: &Path) -> Result<Vec<Claim>, &'static str> {
    let mut text = String::new();
    let handle = match std::fs::File::open(file) {
        Ok(handle) => handle,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("cannot be opened"),
    };
    if handle
        .take(MAX_BYTES + 1)
        .read_to_string(&mut text)
        .is_err()
    {
        return Err("cannot be read as text");
    }
    if text.len() as u64 > MAX_BYTES {
        return Err("is larger than 1 MiB");
    }
    match serde_json::from_str::<Claims>(&text) {
        Ok(claims) if claims.schema == SCHEMA => Ok(claims.claims),
        Ok(_) => Err("names another schema"),
        Err(_) => Err("is not a bay.claims/1 file"),
    }
}

/// Replaces `file` with `claims` by atomic rename; removes it when none are left.
fn replace(file: &Path, claims: Vec<Claim>) {
    if claims.is_empty() {
        let _ = std::fs::remove_file(file);
        return;
    }
    let Some(dir) = file.parent() else {
        return;
    };
    let Ok(mut text) = serde_json::to_string(&Claims {
        schema: SCHEMA.to_owned(),
        claims,
    }) else {
        return;
    };
    text.push('\n');
    let temporary = dir.join(format!("{FILE}.{}.tmp", std::process::id()));
    if std::fs::create_dir_all(dir).is_ok()
        && std::fs::write(&temporary, text).is_ok()
        && crate::durable::rename(&temporary, file).is_err()
    {
        let _ = std::fs::remove_file(&temporary);
    }
}

/// Holds the claims lock of the directory `dir` while it lives: hook events of two sessions that
/// update one file at once do not lose each other's claims. A lock older than five seconds is a
/// dead hook's and is taken over; one that stays held past half a second is not waited for (a
/// hook never blocks a session for long).
struct Lock(Option<PathBuf>);

impl Lock {
    fn take(dir: &Path) -> Self {
        let path = dir.join(LOCK);
        if std::fs::create_dir_all(dir).is_err() {
            return Self(None);
        }
        for _ in 0..100 {
            if std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .is_ok()
            {
                return Self(Some(path));
            }
            let stale = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age > std::time::Duration::from_secs(5));
            if stale {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Self(None)
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Whether `claim` holds its path against `session` at `now`: another session's, and live.
fn holds(claim: &Claim, session: Option<&str>, now: u64) -> bool {
    Some(claim.session.as_str()) != session && claim.deadline_s > now
}

/// A write the claims could not judge: the claims file of its tree cannot be read (ruling R5:
/// allowed with a `COVERAGE_UNKNOWN` notice, recorded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreadable {
    /// The claims file.
    pub file: PathBuf,
    /// The path in its tree an edit tool's write names; none for a shell command, whose words
    /// bay cannot tell a write among.
    pub path: Option<String>,
    pub why: &'static str,
}

/// The claims judgement of one `pre-tool` call: each tree's claims file read once, and the
/// first one that could not be read.
#[derive(Default)]
struct Judge {
    read: Vec<(Tree, Vec<Claim>)>,
    unreadable: Option<Unreadable>,
}

impl Judge {
    /// The live claim of another session than `session` on the resolved path `target`.
    fn held(&mut self, target: &Path, session: Option<&str>, now: u64) -> Option<Held> {
        let tree = tree_of(target)?;
        let path = relative(&tree, target)?;
        let known = self.read.iter().position(|(known, _)| *known == tree);
        let at = known.unwrap_or_else(|| {
            let file = tree.file();
            let claims = judged(&file).unwrap_or_else(|why| {
                self.unreadable.get_or_insert(Unreadable {
                    file,
                    path: Some(path.clone()),
                    why,
                });
                Vec::new()
            });
            self.read.push((tree, claims));
            self.read.len() - 1
        });
        self.read[at]
            .1
            .iter()
            .find(|claim| claim.path == path && holds(claim, session, now))
            .map(|claim| Held {
                path,
                holder: claim.session.clone(),
                deadline_s: claim.deadline_s,
            })
    }

    /// What the judgement found: a live claim, else whether a claims file could not be read.
    fn verdict(self, held: Option<Held>) -> Result<Option<Held>, Unreadable> {
        match (held, self.unreadable) {
            (None, Some(unreadable)) => Err(unreadable),
            (held, _) => Ok(held),
        }
    }
}

/// The live claim of another session than `session` on `file` (absolute, or relative to `cwd`)
/// at `now`, from the claims file of the file's tree.
///
/// # Errors
///
/// That claims file cannot be read, so the write is not judged (ruling R5).
pub fn held(
    cwd: &Path,
    file: &Path,
    session: Option<&str>,
    now: u64,
) -> Result<Option<Held>, Unreadable> {
    let Some(target) = crate::coord::resolve(&cwd.join(file)) else {
        return Ok(None);
    };
    let mut judge = Judge::default();
    let held = judge.held(&target, session, now);
    judge.verdict(held)
}

/// The live claim of another session than `session` on a path a shell `command` run in `cwd`
/// names: each of its words (also in a shell's `-c` script, `crate::deny::words`), without a
/// redirection's operator (`>file`, `2>>file`) or an option's name (`--out=file`), taken as a
/// path relative to `cwd`. Best effort: bay never evaluates a shell, so an expansion, a glob or a
/// `cd` before the write is not followed (`DESIGN` § 11c). A live claim on any word wins over a
/// claims file that cannot be read.
///
/// # Errors
///
/// No word names a held path, and the claims file of a word's tree cannot be read.
pub fn held_by_command(
    cwd: &Path,
    command: &str,
    session: Option<&str>,
    now: u64,
) -> Result<Option<Held>, Unreadable> {
    let Some(cwd) = crate::coord::resolve(cwd) else {
        return Ok(None);
    };
    let mut judge = Judge::default();
    let held = crate::deny::words(command)
        .iter()
        .take(MAX_WORDS)
        .filter_map(|word| path_word(word))
        .find_map(|word| {
            let target = crate::coord::resolve(&cwd.join(word))?;
            judge.held(&target, session, now)
        });
    judge.verdict(held).map_err(|unreadable| Unreadable {
        path: None,
        ..unreadable
    })
}

/// The path a shell word may name, if any.
fn path_word(word: &str) -> Option<&str> {
    let word = if word.starts_with('-') {
        word.split_once('=')?.1
    } else {
        word.trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches('&')
            .trim_start_matches(['>', '<'])
    };
    (!word.is_empty()).then_some(word)
}

/// Claims `targets` (absolute, or relative to `cwd`) for `session` until `now + ttl_s`, each in
/// its own tree's claims file, and moves the deadline of the session's other claims in those
/// trees to the same time. A path another live session holds is not claimed. Expired claims are
/// dropped. The files written are remembered in `state_dir`, for [`release`].
pub fn claim(
    cwd: &Path,
    targets: &[PathBuf],
    session: &str,
    now: u64,
    ttl_s: u64,
    state_dir: &Path,
) {
    let mut by_tree: Vec<(Tree, Vec<String>)> = Vec::new();
    for target in targets {
        let Some(target) = crate::coord::resolve(&cwd.join(target)) else {
            continue;
        };
        let Some(tree) = tree_of(&target) else {
            continue;
        };
        let Some(path) = relative(&tree, &target) else {
            continue;
        };
        match by_tree.iter_mut().find(|(known, _)| *known == tree) {
            Some((_, paths)) => paths.push(path),
            None => by_tree.push((tree, vec![path])),
        }
    }
    let deadline_s = now.saturating_add(ttl_s);
    let mut written = Vec::new();
    for (tree, paths) in by_tree {
        let file = tree.file();
        let Some(dir) = file.parent() else {
            continue;
        };
        let _lock = Lock::take(dir);
        let mut claims: Vec<Claim> = read(&file)
            .into_iter()
            .filter(|claim| claim.deadline_s > now)
            .collect();
        for claim in &mut claims {
            if claim.session == session {
                claim.deadline_s = deadline_s;
            }
        }
        for path in paths {
            if claims.iter().any(|claim| claim.path == path) {
                continue;
            }
            claims.push(Claim {
                path,
                session: session.to_owned(),
                deadline_s,
            });
        }
        claims.sort_by(|a, b| a.path.cmp(&b.path));
        replace(&file, claims);
        written.push(file);
    }
    remember(state_dir, &written);
}

/// Adds `files` to the claims files `state_dir` lists.
fn remember(state_dir: &Path, files: &[PathBuf]) {
    let mut known = remembered(state_dir);
    let before = known.len();
    for file in files {
        if !known.contains(file) {
            known.push(file.clone());
        }
    }
    if known.len() == before {
        return;
    }
    let mut text = String::new();
    for file in known.iter().filter_map(|file| file.to_str()) {
        text.push_str(file);
        text.push('\n');
    }
    if std::fs::create_dir_all(state_dir).is_ok() {
        let _ = std::fs::write(state_dir.join(STATE_FILE), text);
    }
}

fn remembered(state_dir: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(state_dir.join(STATE_FILE))
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Releases every claim of `session` in the claims files `state_dir` lists (the session's stop
/// or end).
pub fn release(session: &str, state_dir: &Path) {
    for file in remembered(state_dir) {
        let Some(dir) = file.parent() else {
            continue;
        };
        let _lock = Lock::take(dir);
        let claims: Vec<Claim> = read(&file)
            .into_iter()
            .filter(|claim| claim.session != session)
            .collect();
        replace(&file, claims);
    }
    let _ = std::fs::remove_file(state_dir.join(STATE_FILE));
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Claim, held, held_by_command, path_word, read};

    fn tree(dir: &Path) -> std::path::PathBuf {
        let root = dir.canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".git/bay")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        root
    }

    fn write_claims(root: &Path, claims: &[Claim]) {
        let text = serde_json::json!({"schema": super::SCHEMA, "claims": claims});
        std::fs::write(root.join(".git/bay/claims.json"), text.to_string()).unwrap();
    }

    #[test]
    fn a_word_names_a_path_without_its_redirection_or_option() {
        assert_eq!(path_word(">src/a.rs"), Some("src/a.rs"));
        assert_eq!(path_word("2>>src/a.rs"), Some("src/a.rs"));
        assert_eq!(path_word("&>src/a.rs"), Some("src/a.rs"));
        assert_eq!(path_word("--output=src/a.rs"), Some("src/a.rs"));
        assert_eq!(path_word("-i"), None);
        assert_eq!(path_word(">"), None);
    }

    /// `DESIGN` § 11c: `PreToolUse` reads the claims file in under 10 ms and does not fetch. The
    /// judgement is file-system calls only; here, of a tree that holds many claims.
    #[test]
    fn a_judgement_reads_the_claims_file_in_under_ten_ms() {
        let dir = tempfile::tempdir().unwrap();
        let root = tree(dir.path());
        let claims: Vec<Claim> = (0..1000)
            .map(|n| Claim {
                path: format!("src/f{n}.rs"),
                session: "s-holder".to_owned(),
                deadline_s: u64::MAX,
            })
            .collect();
        write_claims(&root, &claims);
        assert_eq!(read(&root.join(".git/bay/claims.json")).len(), 1000);
        // Each judgement on its own, the fastest of several runs: a loaded box stalls a run.
        let fastest = |judge: &dyn Fn() -> bool| {
            (0..20)
                .map(|_| {
                    let begun = std::time::Instant::now();
                    assert!(judge());
                    begun.elapsed()
                })
                .min()
                .unwrap()
        };
        let edit = fastest(&|| {
            held(&root, Path::new("src/f999.rs"), Some("s-other"), 0)
                .unwrap()
                .is_some()
        });
        let shell = fastest(&|| {
            held_by_command(&root, "echo x > src/f999.rs", Some("s-other"), 0)
                .unwrap()
                .is_some()
        });
        assert!(
            edit.max(shell) < std::time::Duration::from_millis(10),
            "{edit:?} {shell:?}"
        );
    }

    #[test]
    fn a_held_path_needs_another_session_and_a_live_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let root = tree(dir.path());
        let claim = |deadline_s| Claim {
            path: "src/a.rs".to_owned(),
            session: "s-holder".to_owned(),
            deadline_s,
        };
        write_claims(&root, &[claim(100)]);
        let file = Path::new("src/a.rs");
        assert!(held(&root, file, Some("s-other"), 99).unwrap().is_some());
        assert!(held(&root, file, None, 99).unwrap().is_some());
        assert!(held(&root, file, Some("s-holder"), 99).unwrap().is_none());
        assert!(held(&root, file, Some("s-other"), 100).unwrap().is_none());
        assert!(
            held(&root, Path::new("src/b.rs"), Some("s-other"), 99)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_corrupt_claims_file_holds_nothing_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let root = tree(dir.path());
        let file = root.join(".git/bay/claims.json");
        std::fs::write(&file, "not json").unwrap();
        let unreadable = held(&root, Path::new("src/a.rs"), Some("s-other"), 0).unwrap_err();
        assert_eq!(unreadable.file, file);
        assert_eq!(unreadable.path.as_deref(), Some("src/a.rs"));
        assert!(held_by_command(&root, "echo x > src/a.rs", Some("s-other"), 0).is_err());
        // No claims file is no claim, and not an unknown.
        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            held(&root, Path::new("src/a.rs"), Some("s-other"), 0),
            Ok(None)
        );
    }
}

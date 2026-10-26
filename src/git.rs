//! Thin wrapper around the `git` executable.
//!
//! Bay shells out to `git` instead of linking libgit2 or gitoxide, so it
//! handles every repository format the installed Git does. Git 3.0 makes
//! SHA-256 object IDs and the reftable ref backend the defaults. Never assume
//! 40-character object IDs, and never read `.git/refs` directly; ask Git.
//!
//! Every call stops at [`crate::result::GIT_CALL_TIMEOUT`].
//!
//! G171: a test build counts its Git spawns. With `BAY_TEST_GIT_TRACE` naming a file, each
//! `git` this process runs appends one JSON line there when it ends: its subcommand, its
//! duration in microseconds from spawn to exit, and its exit code (`null` when killed). A
//! release build (`BAY_RELEASE_VERSION` set at build time) never reads the variable.
//!
//! G200: a handle lists its repository's config once ([`Git::config_get`]) and answers bay's
//! own keys from that list until any `git config` write in this process; the common
//! directory of a directory is asked once per process ([`Git::common_dir`]).
//!
//! G104b: no call fetches an object on its own. In a partial clone (the shared blob-less
//! cache) Git would fetch any object a read touches and the store lacks, one round trip each,
//! so a walk or a diff over undeclared paths would pull their blobs onto the machine (G212).
//! Every call runs with `GIT_NO_LAZY_FETCH=1`: a missing object is an error bay reports. A read
//! that names its objects (`cat-file <type> <name>`, `cat-file --batch` of names, `notes show`)
//! asks for them by name: when the store lacks one, bay fetches exactly those objects in one
//! `git fetch --stdin` from the promisor remote, the form Git's own promisor code uses, and
//! reads again once. A handle made by [`Git::fetching`] lets Git fetch what it writes or
//! merges: a view's declared files, a merge, a materialized tree. Git before 2.45 ignores the
//! variable; the shared cache needs 2.48 (`GIT-FEATURES` § 1), so below it no store should
//! have a promisor remote, and one that does fetches lazily as before.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::Instant;

use crate::result::{GIT_CALL_TIMEOUT, GIT_NETWORK_TIMEOUT};

/// The options before a call that writes a work tree ([`Git::writing_tree`]): no CRLF
/// conversion of the box's `core.autocrlf`, and long paths on Git for Windows (a key other Gits
/// do not read).
pub const TREE_WRITES: [&str; 4] = ["-c", "core.autocrlf=false", "-c", "core.longpaths=true"];

pub struct Git {
    dir: PathBuf,
    /// `dir` is itself a bare repository. Git 3.0 sets `safe.bareRepository=explicit`,
    /// which stops discovering a bare repository from `-C`, so bay names it with `--git-dir`.
    bare_dir: bool,
    /// G200: the config as this handle last listed it, for [`Git::config_get`].
    config: Mutex<Option<Snapshot>>,
    /// G104b: Git may fetch an absent object as this handle's calls read it ([`Git::fetching`]).
    lazy_fetch: bool,
    /// This handle's calls write a work tree byte for byte ([`Git::writing_tree`]).
    writes_tree: bool,
}

/// G200: one `git config --list` of a handle, every entry in Git's order (the last value
/// wins), read when [`CONFIG_WRITES`] stood at `writes`.
struct Snapshot {
    writes: u64,
    entries: Vec<ConfigEntry>,
    /// Each config file the listing read, and the global ones that may appear, with what they
    /// were when listed: another process's `git config` (a key set while an executor runs)
    /// replaces its file, so a changed file lists again.
    files: Vec<(PathBuf, Option<FileStamp>)>,
}

/// What a config file was: its length, modification time and, on Unix, its inode (Git writes
/// a config file by renaming a new one over it).
type FileStamp = (u64, Option<std::time::SystemTime>, u64);

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    Some((meta.len(), meta.modified().ok(), inode))
}

impl Snapshot {
    fn changed(&self) -> bool {
        self.files
            .iter()
            .any(|(path, stamp)| file_stamp(path) != *stamp)
    }
}

/// The global config files Git reads, whether or not they exist yet.
fn global_config_files() -> Vec<PathBuf> {
    if let Some(global) = std::env::var_os("GIT_CONFIG_GLOBAL") {
        return vec![PathBuf::from(global)];
    }
    let home = crate::platform::home().map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".config")));
    home.map(|home| home.join(".gitconfig"))
        .into_iter()
        .chain(xdg.map(|xdg| xdg.join("git/config")))
        .collect()
}

struct ConfigEntry {
    /// The key as Git lists it: section and name in lower case, a subsection as written.
    key: Vec<u8>,
    /// `None` for a key written with no `=`, which `git config --get` prints as empty.
    value: Option<Vec<u8>>,
    /// Written in the repository's own config file, which alone `git config --local` reads
    /// (it follows no include).
    local: bool,
}

/// G200: how many `git config` calls that may write this process has run. A snapshot read
/// before the latest one is read again.
static CONFIG_WRITES: AtomicU64 = AtomicU64::new(0);

/// G200: the common directory of each directory a handle asked, by the path it was given.
static COMMON_DIRS: Mutex<BTreeMap<PathBuf, String>> = Mutex::new(BTreeMap::new());

/// Formats and location of a repository, as reported by Git.
pub struct RepoInfo {
    /// The work tree, or the git directory of a bare repository.
    pub toplevel: String,
    pub bare: bool,
    /// `sha1` or `sha256`.
    pub object_format: String,
    /// `files` or `reftable`.
    pub ref_format: String,
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
}

#[derive(Debug)]
pub enum GitError {
    Spawn(std::io::Error),
    Timeout { args: String, secs: u64 },
    Failed { args: String, stderr: String },
    NotUtf8 { args: String },
    UnexpectedOutput(&'static str),
    Join,
}

impl GitError {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Spawn(error) => format!("failed to run git: {error}"),
            Self::Timeout { args, secs } => format!("{args} exceeded {secs}s"),
            Self::Failed { args, stderr } => format!("git {args} failed: {stderr}"),
            Self::NotUtf8 { args } => format!("git {args} returned output that is not utf-8"),
            Self::UnexpectedOutput(what) => (*what).to_owned(),
            Self::Join => "git reader thread failed".to_owned(),
        }
    }
}

impl Git {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let bare_dir = is_bare_dir(&dir);
        Self {
            dir,
            bare_dir,
            config: Mutex::new(None),
            lazy_fetch: false,
            writes_tree: false,
        }
    }

    /// G104b, G212: a handle on the same repository whose calls let Git fetch an absent object
    /// as they read it, for a call that means to: writing a view's declared files, a merge,
    /// materializing a tree. Every other call of bay reads only what the store holds.
    #[must_use]
    pub fn fetching(&self) -> Self {
        Self {
            dir: self.dir.clone(),
            bare_dir: self.bare_dir,
            config: Mutex::new(None),
            lazy_fetch: true,
            writes_tree: self.writes_tree,
        }
    }

    /// A handle on the same repository whose calls write a work tree as the tree holds it, on
    /// every platform (plan W5): [`TREE_WRITES`] before each call, so no `core.autocrlf` of the
    /// box turns a checked-out file into other bytes than its blob, and Git for Windows writes
    /// a path longer than 260 characters. For a view's checkout, its sparse set and its reset.
    #[must_use]
    pub fn writing_tree(&self) -> Self {
        Self {
            dir: self.dir.clone(),
            bare_dir: self.bare_dir,
            config: Mutex::new(None),
            lazy_fetch: self.lazy_fetch,
            writes_tree: true,
        }
    }

    /// `git config --get <key>`: the last value of `key` in the config Git reads here (a key
    /// with no value is empty), `None` when it is unset. A key of bay's own section is answered
    /// from this handle's snapshot, listed once and again after any `git config` write this
    /// process runs; Git itself never writes bay's keys. Any other key, or a snapshot Git
    /// cannot list, asks Git for that key alone.
    ///
    /// # Errors
    ///
    /// See [`Git::run`]; a value that is not UTF-8 is [`GitError::NotUtf8`].
    pub fn config_get(&self, key: &str) -> Result<Option<String>, GitError> {
        self.config_value(key, false)
    }

    /// [`Git::config_get`] of the repository's own config file only, as
    /// `git config --local --get <key>` reads it.
    ///
    /// # Errors
    ///
    /// See [`Git::config_get`].
    pub fn config_get_local(&self, key: &str) -> Result<Option<String>, GitError> {
        self.config_value(key, true)
    }

    fn config_value(&self, key: &str, local: bool) -> Result<Option<String>, GitError> {
        let ask_git = || -> Result<Option<String>, GitError> {
            let mut args = vec!["config"];
            if local {
                args.push("--local");
            }
            args.extend(["--get", key]);
            match self.run_status(&args)? {
                (0, value, _) => Ok(Some(value)),
                // Exit 1 with nothing on stderr: the key is unset.
                (1, _, stderr) if stderr.is_empty() => Ok(None),
                (_, _, stderr) => Err(GitError::Failed {
                    args: args.join(" "),
                    stderr,
                }),
            }
        };
        let Some(wanted) = bay_key(key) else {
            return ask_git();
        };
        let mut slot = self.config.lock().unwrap_or_else(PoisonError::into_inner);
        let writes = CONFIG_WRITES.load(Ordering::SeqCst);
        if slot
            .as_ref()
            .is_none_or(|snapshot| snapshot.writes != writes || snapshot.changed())
        {
            // A config Git cannot list (a broken global file, say) may still answer `--local`,
            // or fail as the one key's read does: Git says which.
            let Ok(snapshot) = self.list_config(writes) else {
                return ask_git();
            };
            *slot = Some(snapshot);
        }
        let Some(snapshot) = slot.as_ref() else {
            return ask_git();
        };
        let Some(entry) = snapshot
            .entries
            .iter()
            .rev()
            .find(|entry| entry.key == wanted && (entry.local || !local))
        else {
            return Ok(None);
        };
        let value = String::from_utf8(entry.value.clone().unwrap_or_default()).map_err(|_| {
            GitError::NotUtf8 {
                args: format!("config --get {key}"),
            }
        })?;
        Ok(Some(value.trim_end().to_owned()))
    }

    /// Lists the config Git reads here, with each entry's scope and file.
    fn list_config(&self, writes: u64) -> Result<Snapshot, GitError> {
        let out = self.run_bytes(
            &["config", "--list", "--show-scope", "--show-origin", "-z"],
            &[],
            None,
        )?;
        let mut fields: Vec<&[u8]> = out.split(|&byte| byte == 0).collect();
        if fields.last().is_some_and(|last| last.is_empty()) {
            fields.pop();
        }
        if !fields.len().is_multiple_of(3) {
            return Err(GitError::UnexpectedOutput(
                "unexpected git config --list output",
            ));
        }
        // The repository's own file is where its first entry comes from: an include is itself
        // an entry of the file that holds it, listed before what it includes.
        let own = fields
            .chunks(3)
            .find(|entry| entry[0] == b"local")
            .map(|entry| entry[1]);
        let entries = fields
            .chunks(3)
            .map(|entry| {
                let (key, value) = match entry[2].iter().position(|&byte| byte == b'\n') {
                    Some(at) => (&entry[2][..at], Some(entry[2][at + 1..].to_vec())),
                    None => (entry[2], None),
                };
                ConfigEntry {
                    key: key.to_vec(),
                    value,
                    local: entry[0] == b"local" && Some(entry[1]) == own,
                }
            })
            .collect();
        let base = if self.bare_dir {
            std::env::current_dir().unwrap_or_default()
        } else {
            self.dir.clone()
        };
        let mut paths: Vec<PathBuf> = fields
            .chunks(3)
            .filter_map(|entry| entry[1].strip_prefix(b"file:"))
            .filter_map(|path| std::str::from_utf8(path).ok())
            .map(|path| base.join(path))
            .chain(global_config_files())
            .collect();
        paths.sort();
        paths.dedup();
        let files = paths
            .into_iter()
            .map(|path| {
                let stamp = file_stamp(&path);
                (path, stamp)
            })
            .collect();
        Ok(Snapshot {
            writes,
            entries,
            files,
        })
    }

    /// `rev-parse --path-format=absolute --git-common-dir`, asked once per process for each
    /// directory a handle is given: a directory's repository does not move while bay runs. A
    /// directory that is gone asks Git again, so its error is Git's.
    ///
    /// # Errors
    ///
    /// See [`Git::run`].
    pub fn common_dir(&self) -> Result<String, GitError> {
        if self.dir.is_dir()
            && let Some(known) = COMMON_DIRS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&self.dir)
        {
            return Ok(known.clone());
        }
        let common = self.run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        COMMON_DIRS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(self.dir.clone(), common.clone());
        Ok(common)
    }

    /// Runs `git <args>` and returns trimmed stdout.
    ///
    /// # Errors
    ///
    /// The git binary is missing, the call exceeds [`GIT_CALL_TIMEOUT`], or git exits non-zero.
    pub fn run(&self, args: &[&str]) -> Result<String, GitError> {
        self.run_with(args, &[], None)
    }

    /// Runs `git <args>` with extra environment and optional stdin; returns trimmed stdout.
    ///
    /// # Errors
    ///
    /// See [`Git::run`].
    pub fn run_with(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
        input: Option<&[u8]>,
    ) -> Result<String, GitError> {
        let stdout = self.run_bytes(args, envs, input)?;
        let stdout = String::from_utf8(stdout).map_err(|_| GitError::NotUtf8 {
            args: args.join(" "),
        })?;
        Ok(stdout.trim_end().to_owned())
    }

    /// [`Git::run_with`], returning stdout as bytes, untrimmed: for object contents, which
    /// need not be text.
    ///
    /// # Errors
    ///
    /// See [`Git::run`].
    pub fn run_bytes(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
        input: Option<&[u8]>,
    ) -> Result<Vec<u8>, GitError> {
        self.run_bytes_for(args, envs, input, GIT_CALL_TIMEOUT)
    }

    /// [`Git::run_bytes`] bounded by `timeout` instead of [`GIT_CALL_TIMEOUT`]: for a call
    /// whose bound the policy states (a check's `materialize_timeout_s`).
    ///
    /// # Errors
    ///
    /// The git binary is missing, the call exceeds `timeout`, or git exits non-zero.
    pub fn run_bytes_for(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
        input: Option<&[u8]>,
        timeout: std::time::Duration,
    ) -> Result<Vec<u8>, GitError> {
        let args_display = args.join(" ");
        let mut output = self.output(args, envs, input, timeout)?;
        // W10: a host may refuse a push of bay's records for a moment of its own (a hosting
        // check it could not finish), and accept the same push a second later. Such a push is
        // tried again before it is called refused; a ref another writer moved is the caller's
        // to read, so it is answered at once.
        for wait in HOST_RETRY_WAITS {
            if output.status.success() || !host_refused(args, &output.stderr) {
                break;
            }
            std::thread::sleep(wait);
            output = self.output(args, envs, input, timeout)?;
        }
        let stderr = String::from_utf8(output.stderr).map_err(|_| GitError::NotUtf8 {
            args: args_display.clone(),
        })?;
        if !output.status.success() {
            return Err(GitError::Failed {
                args: args_display,
                stderr: stderr.trim().to_owned(),
            });
        }
        Ok(output.stdout)
    }

    /// Runs `git <args>` and returns the exit code with stdout and stderr, for commands
    /// whose non-zero exit is an answer (merge-tree's conflict exit, for example).
    ///
    /// # Errors
    ///
    /// The git binary is missing or the call exceeds the timeout.
    pub fn run_status(&self, args: &[&str]) -> Result<(i32, String, String), GitError> {
        self.run_status_with(args, &[])
    }

    /// [`Self::run_status`] with `envs` set for the call.
    ///
    /// # Errors
    ///
    /// The git binary is missing or the call exceeds the timeout.
    pub fn run_status_with(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Result<(i32, String, String), GitError> {
        let output = self.output(args, envs, None, GIT_CALL_TIMEOUT)?;
        Ok((
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned(),
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }

    /// Runs `git <args>` with `envs` and optional stdin, bounded by `timeout`. G104b: when a
    /// read that names its objects finds one absent from a partial clone, those objects are
    /// fetched by name from the promisor remote and the read runs once more; a failed fetch
    /// leaves the first answer, which says what is missing.
    fn output(
        &self,
        args: &[&str],
        envs: &[(&str, &str)],
        input: Option<&[u8]>,
        timeout: std::time::Duration,
    ) -> Result<Output, GitError> {
        let timeout = bound_for(args, timeout);
        let args_display = args.join(" ");
        let run = || {
            let mut cmd = self.command();
            cmd.args(as_formatted(args));
            for (key, value) in envs {
                cmd.env(key, value);
            }
            run_bounded_for(cmd, &args_display, input, timeout)
        };
        let mut output = run()?;
        if self.lazy_fetch {
            return Ok(output);
        }
        let absent = named_absent(args, &output);
        if absent.is_empty() {
            return Ok(output);
        }
        match self.fetch_named(&absent, timeout) {
            None => Ok(output),
            Some(Ok(())) => run(),
            // A `--batch` read answers `<name> missing` and exits 0 for a path its tree lacks and
            // for a blob the store lacks alike: one the tree names that no fetch could bring is
            // an error, never read as absent.
            Some(Err(why)) if output.status.success() && args.contains(&"--batch") => {
                Err(GitError::Failed {
                    args: args.join(" "),
                    stderr: why.trim().to_owned(),
                })
            }
            Some(Err(why)) => {
                output.stderr.extend_from_slice(why.as_bytes());
                Ok(output)
            }
        }
    }

    /// G104b: fetches the objects `names` name from the promisor remote in one explicit
    /// `fetch --stdin`. `None` in a full clone, or when no name resolves to an object id (a
    /// path the tree lacks); the reason, to add to Git's own, when the fetch fails.
    fn fetch_named(
        &self,
        names: &[String],
        timeout: std::time::Duration,
    ) -> Option<Result<(), String>> {
        let remote = self.promisor_remote()?;
        // A name is resolved through trees, which a blob-less store holds: no blob is read.
        let mut ids = String::new();
        let mut resolved = Vec::new();
        for name in names {
            if let Ok(oid) = self.run(&["rev-parse", "--verify", "-q", name]) {
                ids.push_str(&oid);
                ids.push('\n');
                resolved.push(name.as_str());
            }
        }
        if ids.is_empty() {
            return None;
        }
        let fetched = self.run_bytes_for(
            &[
                "-c",
                "fetch.negotiationAlgorithm=noop",
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--recurse-submodules=no",
                "--filter=blob:none",
                "--stdin",
                &remote,
            ],
            &[],
            Some(ids.as_bytes()),
            timeout,
        );
        Some(fetched.map(|_| ()).map_err(|error| {
            format!(
                "\n{} absent from this partial clone, and fetching it by id from its promisor remote {remote} failed: {}",
                resolved.join(", "),
                error.message()
            )
        }))
    }

    /// The remote a partial clone fetches its absent objects from, from this handle's config
    /// snapshot: `extensions.partialClone`, else the first remote whose `remote.<name>.promisor`
    /// is true. `None` in a full clone, or for a name Git would read as an option.
    fn promisor_remote(&self) -> Option<String> {
        let mut slot = self.config.lock().unwrap_or_else(PoisonError::into_inner);
        let writes = CONFIG_WRITES.load(Ordering::SeqCst);
        if slot
            .as_ref()
            .is_none_or(|snapshot| snapshot.writes != writes)
        {
            *slot = Some(self.list_config(writes).ok()?);
        }
        let snapshot = slot.as_ref()?;
        let text = |entry: &ConfigEntry| {
            String::from_utf8_lossy(entry.value.as_deref().unwrap_or_default())
                .trim()
                .to_owned()
        };
        let named = snapshot
            .entries
            .iter()
            .rev()
            .find(|entry| entry.key == b"extensions.partialclone")
            .map(text)
            .or_else(|| {
                // Each remote's last `promisor` setting wins; the first remote it makes one.
                let mut remotes: Vec<(&[u8], bool)> = Vec::new();
                for entry in &snapshot.entries {
                    let Some(name) = entry
                        .key
                        .strip_prefix(b"remote.")
                        .and_then(|key| key.strip_suffix(b".promisor"))
                    else {
                        continue;
                    };
                    let on = entry.value.is_none() || config_true(&text(entry));
                    match remotes.iter_mut().find(|(known, _)| *known == name) {
                        Some(known) => known.1 = on,
                        None => remotes.push((name, on)),
                    }
                }
                remotes
                    .into_iter()
                    .find(|(_, on)| *on)
                    .map(|(name, _)| String::from_utf8_lossy(name).into_owned())
            })?;
        (!named.is_empty() && !named.starts_with('-')).then_some(named)
    }

    /// `git` aimed at this repository, with no arguments yet: for a call that streams its
    /// output instead of collecting it, and so bounds itself.
    #[must_use]
    pub fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        if self.bare_dir {
            cmd.arg(format!("--git-dir={}", self.dir.display()));
        } else {
            cmd.arg("-C").arg(&self.dir);
        }
        if self.writes_tree {
            cmd.args(TREE_WRITES);
        }
        // G104: bay reads some of Git's answers by their words, so every call runs in the C
        // locale (which also stops `LANGUAGE` from translating them) and never prompts.
        cmd.env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C");
        // G104b: Git fetches no absent object on its own, unless this handle means it to.
        cmd.env("GIT_NO_LAZY_FETCH", if self.lazy_fetch { "0" } else { "1" });
        cmd
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether `rev` names an existing object.
    #[must_use]
    pub fn exists(&self, rev: &str) -> bool {
        self.run(&["cat-file", "-e", rev]).is_ok()
    }

    /// Whether `ancestor` is an ancestor of `descendant`, or the same commit, as
    /// `git merge-base --is-ancestor` answers: exit 0 is yes and exit 1 is no. Every other
    /// answer, a missing object among them, is an error and never "no". bay's one reading of
    /// ancestry.
    ///
    /// # Errors
    ///
    /// Git cannot run, or it exits other than 0 or 1 (an absent object, a name that is no
    /// commit).
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, String> {
        let (code, _, stderr) = self
            .run_status(&["merge-base", "--is-ancestor", ancestor, descendant])
            .map_err(|error| error.message())?;
        match code {
            0 => Ok(true),
            1 => Ok(false),
            _ => Err(format!(
                "git merge-base --is-ancestor {ancestor} {descendant}: {stderr}"
            )),
        }
    }

    /// The object a ref points at, or `None` when the ref does not exist.
    ///
    /// # Errors
    ///
    /// See [`Git::run`].
    pub fn ref_target(&self, name: &str) -> Result<Option<String>, GitError> {
        match self.run(&["rev-parse", "--verify", "-q", &format!("{name}^{{object}}")]) {
            Ok(oid) => Ok(Some(oid)),
            Err(GitError::Failed { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The text of the file at `path` in the tree of `commit`, or `None` when that tree has no
    /// such path (or `commit` names no tree this store holds). G104b: a path the tree names
    /// whose blob cannot be read, here or by id from a partial clone's promisor remote, is an
    /// error that names it, never `None`. The tree decides absence; no blob is read for it.
    ///
    /// # Errors
    ///
    /// The tree names the file and its blob cannot be read, or a git fault.
    pub fn file_at(&self, commit: &str, path: &str) -> Result<Option<String>, GitError> {
        let spec = format!("{commit}:{path}");
        match self.run(&["cat-file", "blob", &spec]) {
            Ok(text) => Ok(Some(text)),
            Err(unread @ GitError::Failed { .. }) => {
                match self.run(&["rev-parse", "--verify", "-q", &spec]) {
                    Ok(_) => Err(unread),
                    Err(GitError::Failed { .. }) => Ok(None),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// G200b: the contents of each object `names` names (an object id, or `<commit>:<path>`),
    /// in their order, read by one `git cat-file --batch`: `None` for a name Git answers
    /// `missing` (or `ambiguous`). No spawn when `names` is empty. A caller that must tell a
    /// path its tree lacks from a blob it cannot read asks [`Git::file_at`] for a `None` name.
    ///
    /// # Errors
    ///
    /// A git fault, a name that holds a newline, or an answer this cannot read.
    pub fn objects(&self, names: &[String]) -> Result<Vec<Option<Vec<u8>>>, GitError> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let unreadable = |why: String| GitError::Failed {
            args: "cat-file --batch".to_owned(),
            stderr: why,
        };
        if names.iter().any(|name| name.contains('\n')) {
            return Err(unreadable("an object name holds a newline".to_owned()));
        }
        let mut input = names.join("\n");
        input.push('\n');
        let out = self.run_bytes(&["cat-file", "--batch"], &[], Some(input.as_bytes()))?;
        let mut rest = out.as_slice();
        let mut read = Vec::with_capacity(names.len());
        for name in names {
            let end = rest
                .iter()
                .position(|&byte| byte == b'\n')
                .ok_or_else(|| unreadable(format!("no answer for {name}")))?;
            let header = String::from_utf8_lossy(&rest[..end]).into_owned();
            rest = &rest[end + 1..];
            if header.ends_with(" missing") || header.ends_with(" ambiguous") {
                read.push(None);
                continue;
            }
            let size = header
                .rsplit(' ')
                .next()
                .and_then(|size| size.parse::<usize>().ok())
                .filter(|size| rest.len() > *size && rest[*size] == b'\n')
                .ok_or_else(|| unreadable(format!("a malformed answer for {name}: {header}")))?;
            read.push(Some(rest[..size].to_vec()));
            rest = &rest[size + 1..];
        }
        Ok(read)
    }

    /// # Errors
    ///
    /// See [`Git::run`].
    pub fn version(&self) -> Result<String, GitError> {
        self.run(&["--version"])
    }

    /// # Errors
    ///
    /// See [`Git::run`]. A detached HEAD is not an error: `branch` is `None`.
    pub fn repo_info(&self) -> Result<RepoInfo, GitError> {
        let out = self.run(&[
            "rev-parse",
            "--is-bare-repository",
            "--absolute-git-dir",
            "--show-object-format",
            "--show-ref-format",
        ])?;
        let mut lines = out.lines().map(str::to_owned);
        let mut next = || {
            lines.next().ok_or(GitError::UnexpectedOutput(
                "unexpected git rev-parse output",
            ))
        };
        let (bare, git_dir, object_format, ref_format) =
            (next()? == "true", next()?, next()?, next()?);

        // A bare repository has no work tree; its location is the git directory.
        let toplevel = if bare {
            git_dir
        } else {
            self.run(&["rev-parse", "--show-toplevel"])?
        };

        // `symbolic-ref` works on an unborn branch, unlike `rev-parse --abbrev-ref`.
        // Exit 1 means detached HEAD, which is a reported state, not a fault.
        let branch = match self.run(&["symbolic-ref", "--short", "-q", "HEAD"]) {
            Ok(name) => Some(name),
            Err(GitError::Failed { .. }) => None,
            Err(error) => return Err(error),
        };

        Ok(RepoInfo {
            toplevel,
            bare,
            object_format,
            ref_format,
            branch,
        })
    }

    /// Whether `dir` is outside every Git repository. Git's own discovery message decides, read
    /// in the C locale, and only when no directory Git would search holds a `.git` entry or a
    /// bare repository: Git prints that same message for a `.git` it cannot read (its objects
    /// gone, for example), and that is a damaged repository, never "outside". Any other
    /// failure (a gitfile to a missing admin directory, a `GIT_DIR` that names no repository)
    /// is an error too.
    ///
    /// # Errors
    ///
    /// See [`Git::run`]; also a repository Git found but cannot read.
    pub fn outside_repository(&self) -> Result<bool, GitError> {
        match self.run(&["rev-parse", "--git-dir"]) {
            Ok(_) => Ok(false),
            Err(GitError::Failed { args, stderr })
                if stderr.contains("not a git repository (or any") =>
            {
                match self.damaged_repository() {
                    None => Ok(true),
                    Some(path) => Err(GitError::Failed {
                        args,
                        stderr: format!(
                            "{} holds a repository Git cannot read: {}",
                            path.display(),
                            stderr.trim()
                        ),
                    }),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// The first directory Git's discovery searches, from `dir` up to a
    /// `GIT_CEILING_DIRECTORIES` entry (which Git does not search), that holds a `.git` entry
    /// or is a bare repository.
    fn damaged_repository(&self) -> Option<PathBuf> {
        let start = std::fs::canonicalize(&self.dir).unwrap_or_else(|_| self.dir.clone());
        let ceilings: Vec<PathBuf> = std::env::var_os("GIT_CEILING_DIRECTORIES")
            .map(|value| {
                std::env::split_paths(&value)
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
                    .collect()
            })
            .unwrap_or_default();
        let mut current = Some(start.as_path());
        while let Some(dir) = current {
            if dir != start && ceilings.iter().any(|ceiling| ceiling == dir) {
                return None;
            }
            if std::fs::symlink_metadata(dir.join(".git")).is_ok() || is_bare_dir(dir) {
                return Some(dir.to_path_buf());
            }
            current = dir.parent();
        }
        None
    }

    /// The URL of `remote`, or `None` when no such remote is configured.
    ///
    /// # Errors
    ///
    /// See [`Git::run`].
    pub fn remote_url(&self, remote: &str) -> Result<Option<String>, GitError> {
        match self.run(&["remote", "get-url", remote]) {
            Ok(url) => Ok(Some(url)),
            Err(GitError::Failed { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether `remote` answers a ref listing within the call timeout.
    #[must_use]
    pub fn remote_answers(&self, remote: &str) -> bool {
        self.run(&["ls-remote", "--branches", remote]).is_ok()
    }
}

/// Test hooks are compiled in unless `scripts/release.sh` built this binary.
const TEST_HOOKS: bool = option_env!("BAY_RELEASE_VERSION").is_none();

/// The file `BAY_TEST_GIT_TRACE` names, read once per process; `None` in a release build.
fn trace_file() -> Option<&'static Path> {
    static FILE: OnceLock<Option<PathBuf>> = OnceLock::new();
    FILE.get_or_init(|| {
        std::env::var_os("BAY_TEST_GIT_TRACE")
            .filter(|value| TEST_HOOKS && !value.is_empty())
            .map(PathBuf::from)
    })
    .as_deref()
}

/// G171: appends the trace line of one `git` spawn of `cmd` that started at `started` and
/// ended with `code`, when `BAY_TEST_GIT_TRACE` names a file. Any other program is not
/// traced. The line goes out in one write, so lines from concurrent calls do not interleave.
pub(crate) fn trace(cmd: &Command, started: Instant, code: Option<i32>) {
    let Some(file) = trace_file() else {
        return;
    };
    if cmd.get_program() != "git" {
        return;
    }
    let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    let mut line = serde_json::json!({
        "sub": subcommand(cmd),
        "us": micros,
        "exit": code,
        "pid": std::process::id(),
    })
    .to_string();
    line.push('\n');
    // The trace never changes what bay does: a file that cannot be written is skipped.
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .and_then(|mut out| out.write_all(line.as_bytes()));
}

/// The Git subcommand of `cmd`: its first argument past the global options bay puts before
/// it (`-C <dir>`, `-c <key=value>`, `--git-dir=<dir>` and other `-` options); `--version` is
/// its own. Empty when there is none.
fn subcommand(cmd: &Command) -> String {
    let args = args_of(cmd);
    subcommand_at(&args).map_or_else(String::new, |at| args[at].clone().into_owned())
}

fn args_of(cmd: &Command) -> Vec<Cow<'_, str>> {
    cmd.get_args()
        .map(std::ffi::OsStr::to_string_lossy)
        .collect()
}

/// Where [`subcommand`] finds the subcommand among `args`.
fn subcommand_at(args: &[Cow<'_, str>]) -> Option<usize> {
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        match &**arg {
            "-C" | "-c" => at += 2,
            "--version" => return Some(at),
            text if text.starts_with('-') => at += 1,
            _ => return Some(at),
        }
    }
    None
}

/// G200: whether `cmd` is a `git config` that may write: any that is not a `--get`, `--list`
/// or `get`/`list` read. Every snapshot of [`Git::config_get`] is then read again.
fn writes_config(cmd: &Command) -> bool {
    if cmd.get_program() != "git" {
        return false;
    }
    let args = args_of(cmd);
    let Some(at) = subcommand_at(&args).filter(|&at| args[at] == "config") else {
        return false;
    };
    let rest = &args[at + 1..];
    let read = rest
        .first()
        .is_some_and(|first| first == "get" || first == "list")
        || rest.iter().any(|arg| {
            matches!(
                &**arg,
                "--get" | "--get-all" | "--get-regexp" | "--get-urlmatch" | "--list" | "-l"
            )
        });
    !read
}

/// The key `git config --list` would print for `key` when it is a plain key of bay's own
/// section (`bay.<name>`, no subsection), section and name in lower case; `None` otherwise.
fn bay_key(key: &str) -> Option<Vec<u8>> {
    let (section, name) = key.split_once('.')?;
    let plain = name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    (section.eq_ignore_ascii_case("bay") && plain)
        .then(|| format!("bay.{}", name.to_ascii_lowercase()).into_bytes())
}

/// Whether a config value is one Git reads as boolean true.
fn config_true(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "on")
        || value.parse::<i64>().is_ok_and(|number| number != 0)
}

/// G104b: the names a read of named objects found absent, from its `output`: the name of a
/// failed `cat-file <type> <name>` whose object Git could not read, each `<name> missing`
/// answer of `cat-file --batch` or `--batch-check`, or the note blob a failed `notes show`
/// could not read. Empty for any other call, and for a call that asks Git itself
/// (`--no-lazy-fetch`) which objects are absent.
fn named_absent(args: &[&str], output: &Output) -> Vec<String> {
    if args.contains(&"--no-lazy-fetch") {
        return Vec::new();
    }
    let words: Vec<Cow<'_, str>> = args.iter().map(|arg| Cow::Borrowed(*arg)).collect();
    let Some(at) = subcommand_at(&words) else {
        return Vec::new();
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let failed = !output.status.success();
    match (args[at], &args[at + 1..]) {
        ("cat-file", [kind, name])
            if matches!(*kind, "blob" | "tree" | "commit" | "tag" | "-s" | "-t") =>
        {
            // A path the tree lacks says "does not exist"; an object the store lacks, these.
            let unreadable =
                stderr.contains("bad file") || stderr.contains("could not get object info");
            if failed && unreadable {
                vec![(*name).to_owned()]
            } else {
                Vec::new()
            }
        }
        ("cat-file", ["--batch"]) => batch_missing(&output.stdout, true),
        ("cat-file", [batch]) if batch.starts_with("--batch-check") => {
            batch_missing(&output.stdout, false)
        }
        // A note whose blob the store lacks: `fatal: bad object <note blob>`.
        ("notes", rest) if failed && rest.contains(&"show") => stderr
            .lines()
            .filter_map(|line| line.strip_prefix("fatal: bad object "))
            .map(|oid| oid.trim().to_owned())
            .collect(),
        _ => Vec::new(),
    }
}

/// The names `cat-file --batch` (`contents`) or `--batch-check` answered `<name> missing`. A
/// `--batch` answer is a `<oid> <type> <size>` line, the contents and a newline; one this
/// cannot read ends the scan.
fn batch_missing(out: &[u8], contents: bool) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = out;
    while let Some(end) = rest.iter().position(|&byte| byte == b'\n') {
        let line = String::from_utf8_lossy(&rest[..end]);
        rest = &rest[end + 1..];
        if let Some(name) = line.strip_suffix(" missing") {
            names.push(name.to_owned());
        } else if contents {
            let Some(after) = line
                .rsplit(' ')
                .next()
                .and_then(|size| size.parse::<usize>().ok())
                .and_then(|size| rest.get(size + 1..))
            else {
                break;
            };
            rest = after;
        }
    }
    names
}

/// `args`, with `--no-show-signature` after a `log` or `show`: G104, a box's
/// `log.showSignature` would print each signature's verification among the lines bay parses.
/// And `--no-verify` after a `push` of bay's records alone (G376): a repository's `pre-push`
/// hook guards its branches, and run on bay's own refs it only delays or breaks bay's records,
/// so a push of main still runs it.
fn as_formatted<'a>(args: &'a [&'a str]) -> impl Iterator<Item = &'a str> {
    let first = args.first().copied();
    let quiet = match first {
        Some("log" | "show") => Some("--no-show-signature"),
        Some("push") if records_only(&args[1..]) => Some("--no-verify"),
        _ => None,
    };
    first
        .into_iter()
        .chain(quiet)
        .chain(args.iter().skip(1).copied())
}

/// W10: the waits before a push of bay's records the host refused is tried again.
const HOST_RETRY_WAITS: [std::time::Duration; 2] = [
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(3),
];

/// W10: whether a failed `git <args>` is a push of bay's records alone that the host refused
/// itself (`[remote rejected]`), not one refused because a ref moved (a lease, a lock held by
/// another writer, a fetch first), which its caller reads again.
fn host_refused(args: &[&str], stderr: &[u8]) -> bool {
    if args.first() != Some(&"push") || !records_only(&args[1..]) {
        return false;
    }
    let text = String::from_utf8_lossy(stderr);
    text.contains("[remote rejected]")
        && ![
            "cannot lock ref",
            "stale info",
            "fetch first",
            "non-fast-forward",
            "already exists",
        ]
        .iter()
        .any(|moved| text.contains(moved))
}

/// Whether the words of a `push` after its remote update only bay's records: every
/// destination is under `refs/bay/` or `refs/notes/bay/`, and there is at least one.
fn records_only(words: &[&str]) -> bool {
    let mut names = words.iter().filter(|word| !word.starts_with('-')).skip(1);
    let ours = |name: &&str| {
        let target = name.trim_start_matches('+');
        let target = target.rsplit_once(':').map_or(target, |(_, to)| to);
        target.starts_with(crate::names::BAY) || target.starts_with(crate::names::NOTES)
    };
    let mut any = false;
    names.all(|name| {
        any = true;
        ours(name)
    }) && any
}

/// The bound of a call to `git <args>` given `timeout`: a call that talks to a remote under the
/// default local bound gets [`GIT_NETWORK_TIMEOUT`]; a bound a caller chose stays as it is.
fn bound_for(args: &[&str], timeout: std::time::Duration) -> std::time::Duration {
    if timeout != GIT_CALL_TIMEOUT {
        return timeout;
    }
    let mut words = args.iter();
    while let Some(word) = words.next() {
        if *word == "-c" || *word == "-C" {
            words.next();
            continue;
        }
        if word.starts_with('-') {
            continue;
        }
        return if matches!(*word, "push" | "fetch" | "ls-remote" | "clone" | "pull") {
            GIT_NETWORK_TIMEOUT
        } else {
            timeout
        };
    }
    timeout
}

/// A bare repository holds `HEAD` and `objects/` at its top and has no `.git`.
fn is_bare_dir(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && dir.join("objects").is_dir() && !dir.join(".git").exists()
}

/// Runs `cmd` and collects its output, killing it at [`GIT_CALL_TIMEOUT`].
///
/// # Errors
///
/// The program cannot start, or it exceeds the timeout.
pub fn run_bounded(cmd: Command, display: &str) -> Result<Output, GitError> {
    run_bounded_input(cmd, display, None)
}

/// [`run_bounded`] with `input` on stdin.
///
/// # Errors
///
/// See [`run_bounded`].
pub fn run_bounded_with_input(
    cmd: Command,
    display: &str,
    input: &[u8],
) -> Result<Output, GitError> {
    run_bounded_input(cmd, display, Some(input))
}

fn run_bounded_input(
    cmd: Command,
    display: &str,
    input: Option<&[u8]>,
) -> Result<Output, GitError> {
    run_bounded_for(cmd, display, input, GIT_CALL_TIMEOUT)
}

/// Runs `cmd` with optional stdin, killing it when `timeout` elapses on the monotonic clock.
///
/// # Errors
///
/// The program cannot start, or it exceeds `timeout` ([`GitError::Timeout`]).
pub fn run_bounded_for(
    mut cmd: Command,
    display: &str,
    input: Option<&[u8]>,
    timeout: std::time::Duration,
) -> Result<Output, GitError> {
    let mut child = cmd
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(GitError::Spawn)?;
    let started = Instant::now();

    let stdin_handle = match (input, child.stdin.take()) {
        (Some(bytes), Some(mut stdin)) => {
            let bytes = bytes.to_vec();
            Some(thread::spawn(move || stdin.write_all(&bytes)))
        }
        _ => None,
    };
    let mut stdout = child
        .stdout
        .take()
        .ok_or(GitError::UnexpectedOutput("stdout pipe is missing"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or(GitError::UnexpectedOutput("stderr pipe is missing"))?;
    let stdout_handle = thread::spawn(move || read_to_end(&mut stdout));
    let stderr_handle = thread::spawn(move || read_to_end(&mut stderr));

    let status = wait_until(&mut child, display, timeout);
    // Counted however the call ended: a write that timed out may have landed.
    if writes_config(&cmd) {
        CONFIG_WRITES.fetch_add(1, Ordering::SeqCst);
    }
    trace(
        &cmd,
        started,
        status
            .as_ref()
            .ok()
            .and_then(std::process::ExitStatus::code),
    );
    let status = status?;
    if let Some(handle) = stdin_handle {
        // A child that exits without reading all input closes the pipe; its exit status decides.
        let _ = handle.join();
    }
    Ok(Output {
        status,
        stdout: join_bytes(stdout_handle)?,
        stderr: join_bytes(stderr_handle)?,
    })
}

fn read_to_end(reader: &mut impl Read) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    reader.read_to_end(&mut buffer)?;
    Ok(buffer)
}

fn join_bytes(handle: thread::JoinHandle<std::io::Result<Vec<u8>>>) -> Result<Vec<u8>, GitError> {
    handle
        .join()
        .map_err(|_| GitError::Join)?
        .map_err(GitError::Spawn)
}

fn wait_until(
    child: &mut std::process::Child,
    args: &str,
    timeout: std::time::Duration,
) -> Result<std::process::ExitStatus, GitError> {
    let started = Instant::now();
    // Most git calls end within a few milliseconds, so the first polls come quickly and the
    // pause doubles up to 20 ms: a fixed 20 ms pause cost every call about that much (G171).
    let mut pause = std::time::Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait().map_err(GitError::Spawn)? {
            return Ok(status);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(GitError::Timeout {
                args: args.to_owned(),
                secs: timeout.as_secs(),
            });
        }
        thread::sleep(pause);
        pause = (pause * 2).min(std::time::Duration::from_millis(20));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{Git, GitError, batch_missing, bound_for, run_bounded_for};
    use crate::result::{GIT_CALL_TIMEOUT, GIT_NETWORK_TIMEOUT};
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// A push to a forge with tens of thousands of refs outlasts the local bound (a fleet's
    /// `bay start`, 2026-10-25): every call that talks to a remote gets the network bound, and
    /// a local call, or a bound a caller chose, stays as it is.
    #[test]
    fn a_call_that_talks_to_a_remote_gets_the_network_bound() {
        for args in [
            &["push", "-q", "--atomic", "origin", "a:a"][..],
            &["fetch", "-q", "origin"],
            &["ls-remote", "origin"],
            &["-c", "protocol.version=2", "fetch", "origin"],
            &["clone", "-q", "x", "y"],
        ] {
            assert_eq!(
                bound_for(args, GIT_CALL_TIMEOUT),
                GIT_NETWORK_TIMEOUT,
                "{args:?}"
            );
        }
        for args in [
            &["status"][..],
            &["rev-parse", "HEAD"],
            &["-c", "push.x=1", "log"],
        ] {
            assert_eq!(
                bound_for(args, GIT_CALL_TIMEOUT),
                GIT_CALL_TIMEOUT,
                "{args:?}"
            );
        }
        let chosen = Duration::from_secs(5);
        assert_eq!(bound_for(&["push", "origin"], chosen), chosen);
    }

    /// What `git config [--local] --get <key>` answers in `git`'s repository, spawned.
    fn asked(git: &Git, key: &str, local: bool) -> Option<String> {
        let mut args = vec!["config"];
        if local {
            args.push("--local");
        }
        args.extend(["--get", key]);
        match git.run_status(&args).unwrap() {
            (0, value, _) => Some(value),
            (1, _, stderr) if stderr.is_empty() => None,
            other => panic!("{key}: {other:?}"),
        }
    }

    /// G200: the snapshot answers every read of bay's keys as `git config --get` and
    /// `git config --local --get` do (the last value wins, a key with no value is empty, names
    /// are case-insensitive, `--local` follows no include), and a write through any handle is
    /// seen by the next read.
    #[test]
    fn the_config_snapshot_answers_as_git_does() {
        let dir = tempfile::TempDir::new().unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .arg(dir.path())
            .status()
            .unwrap();
        assert!(init.success());
        let config = dir.path().join(".git").join("config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str(
            "[bay]\n\tmulti = a\n\tmulti = b\n\tflag\n\tMixedCase = X \n\ttrunk = main\n\
             [include]\n\tpath = included\n[bay]\n\tafter = local\n",
        );
        std::fs::write(&config, text).unwrap();
        std::fs::write(
            dir.path().join(".git").join("included"),
            "[bay]\n\ttrunk = included\n\tonlyIncluded = yes\n\tafter = included\n",
        )
        .unwrap();
        let keys = [
            "bay.multi",
            "bay.flag",
            "bay.mixedcase",
            "BAY.MIXEDCASE",
            "bay.trunk",
            "bay.onlyIncluded",
            "bay.after",
            "bay.unset",
        ];
        for handle in [Git::new(dir.path()), Git::new(dir.path().join(".git"))] {
            for key in keys {
                for local in [false, true] {
                    let snapshot = if local {
                        handle.config_get_local(key)
                    } else {
                        handle.config_get(key)
                    };
                    assert_eq!(
                        snapshot.unwrap(),
                        asked(&handle, key, local),
                        "{key} local={local}"
                    );
                }
            }
        }

        let reader = Git::new(dir.path());
        assert_eq!(
            reader.config_get("bay.trunk").unwrap().as_deref(),
            Some("included")
        );
        Git::new(dir.path())
            .run(&["config", "--local", "bay.trunk", "moved"])
            .unwrap();
        assert_eq!(
            reader.config_get_local("bay.trunk").unwrap().as_deref(),
            Some("moved")
        );
        assert_eq!(
            reader.config_get("bay.trunk").unwrap(),
            asked(&reader, "bay.trunk", false)
        );
    }

    /// Ancestry is read one way: an ancestor is `true`, a commit apart is `false`, and a name
    /// Git cannot find is an error, never "not an ancestor".
    #[test]
    fn ancestry_answers_yes_no_or_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let git = Git::new(dir.path());
        git.run(&["init", "-q"]).unwrap();
        let commit = |message: &str| {
            git.run(&[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                message,
            ])
            .unwrap();
            git.run(&["rev-parse", "HEAD"]).unwrap()
        };
        let first = commit("first");
        let second = commit("second");
        assert_eq!(git.is_ancestor(&first, &second), Ok(true));
        assert_eq!(git.is_ancestor(&first, &first), Ok(true));
        assert_eq!(git.is_ancestor(&second, &first), Ok(false));
        let absent = "1".repeat(first.len());
        let missing = git.is_ancestor(&absent, &second);
        assert!(missing.is_err(), "{missing:?}");
        let missing = git.is_ancestor(&first, &absent);
        assert!(missing.is_err(), "{missing:?}");
    }

    /// G104b: the absent names of a `cat-file --batch` answer are its `missing` lines, never a
    /// line of an object's contents, and a `--batch-check` answer's alike.
    #[test]
    fn a_batch_answer_names_only_its_missing_objects() {
        let body = "abc missing\nnot this one missing\n";
        let out = format!(
            "{oid} blob {size}\n{body}\ngone missing\nHEAD:x missing\n",
            oid = "1".repeat(40),
            size = body.len()
        );
        assert_eq!(batch_missing(out.as_bytes(), true), ["gone", "HEAD:x"]);
        let checked = format!("{} blob 3\nHEAD:y missing\n", "2".repeat(40));
        assert_eq!(batch_missing(checked.as_bytes(), false), ["HEAD:y"]);
    }

    /// G135: a call that hangs is killed at its bound and reported as a timeout, never waited
    /// on and never read as a result.
    #[test]
    fn a_hung_call_is_bounded() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 10"]);
        let started = Instant::now();
        let outcome = run_bounded_for(cmd, "hung", None, Duration::from_secs(1));
        assert!(
            matches!(outcome, Err(GitError::Timeout { secs: 1, .. })),
            "{outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    /// G135: a hung call whose own child keeps the output pipe open (as a remote helper or an
    /// ssh would) still returns at the bound: bay never waits for the pipe to close.
    #[test]
    fn a_hung_call_with_a_child_holding_its_pipe_is_bounded() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 10 & sleep 10"]);
        let started = Instant::now();
        let outcome = run_bounded_for(cmd, "hung", None, Duration::from_secs(1));
        assert!(
            matches!(outcome, Err(GitError::Timeout { .. })),
            "{outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    /// Plan W5: a work tree written through [`Git::writing_tree`] holds each blob's bytes,
    /// whatever `core.autocrlf` the box set; written without it, the box's setting converts.
    #[test]
    fn a_tree_written_through_writing_tree_holds_its_blobs_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        let git = Git::new(dir.path());
        git.run(&["init", "-q"]).unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        git.run(&["add", "a.txt"]).unwrap();
        git.run(&[
            "-c",
            "user.name=bay",
            "-c",
            "user.email=bay@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "lf",
        ])
        .unwrap();
        git.run(&["config", "core.autocrlf", "true"]).unwrap();
        let checkout = |git: &Git| {
            std::fs::remove_file(&file).unwrap();
            git.run(&["checkout", "-q", "-f", "HEAD", "--", "a.txt"])
                .unwrap();
            std::fs::read(&file).unwrap()
        };
        assert_eq!(checkout(&git.writing_tree()), b"one\ntwo\n");
        assert_eq!(checkout(&git.fetching().writing_tree()), b"one\ntwo\n");
        assert_eq!(checkout(&git), b"one\r\ntwo\r\n");
        let args: Vec<_> = git
            .writing_tree()
            .command()
            .get_args()
            .map(ToOwned::to_owned)
            .collect();
        assert!(args.ends_with(&super::TREE_WRITES.map(std::ffi::OsString::from)));
    }

    /// G376: only a push whose every destination is one of bay's refs skips the hook.
    #[test]
    fn only_a_push_of_bays_records_skips_the_hook() {
        use super::records_only;
        use crate::names::{BAY, NOTES};
        let change = format!("{BAY}changes/a:{BAY}changes/a");
        let notes = format!("+{NOTES}evidence/x:{NOTES}evidence/x");
        let ours = format!("{BAY}x:{BAY}x");
        assert!(records_only(&["-q", "--atomic", "origin", &change]));
        assert!(records_only(&["origin", &notes]));
        assert!(!records_only(&["origin", "abc:refs/heads/main", &ours]));
        assert!(!records_only(&["origin", "main"]));
        assert!(!records_only(&["origin"]));
    }
}

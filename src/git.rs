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

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::Instant;

use crate::result::GIT_CALL_TIMEOUT;

pub struct Git {
    dir: PathBuf,
    /// `dir` is itself a bare repository. Git 3.0 sets `safe.bareRepository=explicit`,
    /// which stops discovering a bare repository from `-C`, so bay names it with `--git-dir`.
    bare_dir: bool,
    /// G200: the config as this handle last listed it, for [`Git::config_get`].
    config: Mutex<Option<Snapshot>>,
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
        let mut cmd = self.command();
        cmd.args(args);
        for (key, value) in envs {
            cmd.env(key, value);
        }
        let output = run_bounded_for(cmd, &args_display, input, timeout)?;
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
        let args_display = args.join(" ");
        let mut cmd = self.command();
        cmd.args(args);
        let output = run_bounded_input(cmd, &args_display, None)?;
        Ok((
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned(),
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
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
        // G104: bay reads some of Git's answers by their words, so every call runs in the C
        // locale (which also stops `LANGUAGE` from translating them) and never prompts.
        cmd.env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C");
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
        self.run(&["ls-remote", "--heads", remote]).is_ok()
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
    use super::{Git, GitError, run_bounded_for};
    use std::process::Command;
    use std::time::{Duration, Instant};

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
}

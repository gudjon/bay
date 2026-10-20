//! Thin wrapper around the `git` executable.
//!
//! Bay shells out to `git` instead of linking libgit2 or gitoxide, so it
//! handles every repository format the installed Git does. Git 3.0 makes
//! SHA-256 object IDs and the reftable ref backend the defaults. Never assume
//! 40-character object IDs, and never read `.git/refs` directly; ask Git.
//!
//! Every call stops at [`crate::result::GIT_CALL_TIMEOUT`].

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Instant;

use crate::result::GIT_CALL_TIMEOUT;

pub struct Git {
    dir: PathBuf,
    /// `dir` is itself a bare repository. Git 3.0 sets `safe.bareRepository=explicit`,
    /// which stops discovering a bare repository from `-C`, so bay names it with `--git-dir`.
    bare_dir: bool,
}

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
        Self { dir, bare_dir }
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

    let status = wait_until(&mut child, display, timeout)?;
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
        thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{GitError, run_bounded_for};
    use std::process::Command;
    use std::time::{Duration, Instant};

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

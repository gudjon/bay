//! SSH signatures (`DESIGN` § 10a; `GIT-FEATURES`).
//!
//! Commits are signed by Git (`gpg.format=ssh`, `commit-tree -S`) and verified with
//! `git verify-commit` against the class file the commit must verify against; the exit code
//! is the only accept signal, and the principal must equal the actor the commit names.
//! Evidence records are signed with `ssh-keygen -Y sign -n bay-producer` over their canonical
//! JSON. Keys are this machine's: local Git config `bay.agentKey`, `bay.executorKey`,
//! `bay.producerKey`, with `bay.executorName` and `bay.producerName`.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::git::{self, Git, GitError};

/// A key and the principal it signs as.
#[derive(Clone, Debug)]
pub struct Signer {
    pub key: PathBuf,
    pub name: String,
}

impl Signer {
    /// The `-c` options and identity under which Git signs a commit with this key.
    #[must_use]
    pub fn config_args(&self) -> Vec<String> {
        vec![
            "-c".to_owned(),
            "gpg.format=ssh".to_owned(),
            "-c".to_owned(),
            format!("user.signingkey={}", self.key.display()),
        ]
    }

    #[must_use]
    pub fn identity(&self) -> Vec<(String, String)> {
        let email = format!("{}@bay.invalid", self.name);
        vec![
            ("GIT_AUTHOR_NAME".to_owned(), self.name.clone()),
            ("GIT_AUTHOR_EMAIL".to_owned(), email.clone()),
            ("GIT_COMMITTER_NAME".to_owned(), self.name.clone()),
            ("GIT_COMMITTER_EMAIL".to_owned(), email),
        ]
    }
}

/// This machine's signing keys, from local Git config.
#[derive(Clone, Debug)]
pub struct Keys {
    pub agent: Option<PathBuf>,
    pub executor: Option<Signer>,
    pub producer: Option<Signer>,
    /// The operator-chosen names, with or without keys. Every executor needs its own producer
    /// name: one producer name used by two executors collides on one notes ref.
    pub executor_name: String,
    pub producer_name: String,
}

fn config(git: &Git, key: &str) -> Option<String> {
    git.run(&["config", "--get", key])
        .ok()
        .filter(|value| !value.is_empty())
}

impl Keys {
    /// Validates the operator-chosen names (§ 10a).
    ///
    /// # Errors
    ///
    /// The executor or producer name is not a valid bay name.
    pub fn validate(&self) -> Result<(), String> {
        crate::names::operator_name(&self.executor_name)
            .map_err(|why| format!("bay.executorName: {why}"))?;
        crate::names::operator_name(&self.producer_name)
            .map_err(|why| format!("bay.producerName: {why}"))
    }

    #[must_use]
    pub fn load(git: &Git) -> Self {
        let named = |key: &str, name: &str| {
            config(git, key).map(|path| Signer {
                key: PathBuf::from(path),
                name: config(git, name).unwrap_or_else(|| "local".to_owned()),
            })
        };
        Self {
            agent: config(git, "bay.agentKey").map(PathBuf::from),
            executor: named("bay.executorKey", "bay.executorName"),
            producer: named("bay.producerKey", "bay.producerName"),
            executor_name: config(git, "bay.executorName").unwrap_or_else(|| "local".to_owned()),
            producer_name: config(git, "bay.producerName").unwrap_or_else(|| "local".to_owned()),
        }
    }
}

/// Verifies `commit` against `allowed` (one class file). Returns the principal on a good
/// signature, or `None` when it does not verify.
///
/// # Errors
///
/// A git fault other than a failed verification.
pub fn verify_commit(git: &Git, commit: &str, allowed: &Path) -> Result<Option<String>, GitError> {
    let setting = format!("gpg.ssh.allowedSignersFile={}", allowed.display());
    let (code, stdout, stderr) =
        git.run_status(&["-c", &setting, "verify-commit", "--raw", commit])?;
    if code != 0 {
        return Ok(None);
    }
    let text = format!("{stdout}\n{stderr}");
    Ok(text.lines().find_map(|line| {
        let rest = line.split(" signature for ").nth(1)?;
        rest.split(" with ").next().map(str::to_owned)
    }))
}

/// Signs `data` with `key` under `namespace`; returns the armored signature.
///
/// # Errors
///
/// ssh-keygen fails.
pub fn sign_data(key: &Path, namespace: &str, data: &[u8]) -> Result<String, String> {
    let mut cmd = Command::new("ssh-keygen");
    cmd.args(["-Y", "sign", "-q", "-n", namespace, "-f"])
        .arg(key);
    let output = git::run_bounded_with_input(cmd, "ssh-keygen -Y sign", data)
        .map_err(|error| error.message())?;
    if !output.status.success() {
        return Err(format!(
            "ssh-keygen could not sign: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

/// Whether `signature` over `data` verifies for `principal` in `allowed` under `namespace`.
#[must_use]
pub fn verify_data(
    allowed: &Path,
    principal: &str,
    namespace: &str,
    data: &[u8],
    signature: &str,
) -> bool {
    let Ok(dir) = tempdir() else {
        return false;
    };
    let sig_path = dir.join("data.sig");
    if std::fs::write(&sig_path, signature).is_err() {
        return false;
    }
    let mut cmd = Command::new("ssh-keygen");
    cmd.args(["-Y", "verify", "-n", namespace, "-I", principal, "-f"])
        .arg(allowed)
        .arg("-s")
        .arg(&sig_path);
    let verified = git::run_bounded_with_input(cmd, "ssh-keygen -Y verify", data)
        .is_ok_and(|output| output.status.success());
    let _ = std::fs::remove_dir_all(&dir);
    verified
}

fn tempdir() -> std::io::Result<PathBuf> {
    let id = crate::ids::new_change_id()?;
    let dir = std::env::temp_dir().join(format!("bay-verify-{id}"));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

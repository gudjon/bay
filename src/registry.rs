//! The key registry (`DESIGN` § 10a, Registry encoding).
//!
//! `refs/bay/registry` is an append-only chain whose tree holds one OpenSSH `allowed_signers`
//! file per key class: `agent`, `executor`, `producer`, `registry`. The class is the file:
//! a verification names the file of the class it requires, so a key only in the agent file
//! never verifies as an executor. A public key appears in exactly one file, each line has
//! exactly one principal, and options or `cert-authority` lines are refused. Enrolment is
//! plain Git: whoever runs the repository commits the files with a registry key and pushes.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::git::{Git, GitError};

pub const REGISTRY_REF: &str = "refs/bay/registry";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyClass {
    Agent,
    Executor,
    Producer,
    Registry,
}

impl KeyClass {
    pub const ALL: [Self; 4] = [Self::Agent, Self::Executor, Self::Producer, Self::Registry];

    #[must_use]
    pub const fn file(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Executor => "executor",
            Self::Producer => "producer",
            Self::Registry => "registry",
        }
    }

    /// The `ssh-keygen -Y` namespace for objects that are not Git objects.
    #[must_use]
    pub const fn namespace(self) -> &'static str {
        match self {
            Self::Agent => "bay-agent",
            Self::Executor => "bay-executor",
            Self::Producer => "bay-producer",
            Self::Registry => "bay-registry",
        }
    }
}

const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// One registry version: the commit and its class files.
#[derive(Clone, Debug)]
pub struct Registry {
    pub commit: String,
    files: HashMap<KeyClass, String>,
}

impl Registry {
    /// The registry at `commit`, validated.
    ///
    /// # Errors
    ///
    /// A git fault, or a registry that breaks a line rule or puts one key in two places.
    pub fn load_at(git: &Git, commit: &str) -> Result<Self, String> {
        let mut files = HashMap::new();
        for class in KeyClass::ALL {
            let text = match git.run(&["cat-file", "blob", &format!("{commit}:{}", class.file())]) {
                Ok(text) => text,
                Err(GitError::Failed { .. }) => String::new(),
                Err(error) => return Err(error.message()),
            };
            files.insert(class, text);
        }
        let registry = Self {
            commit: commit.to_owned(),
            files,
        };
        registry.validate()?;
        Ok(registry)
    }

    /// The registry this repository holds at `refs/bay/registry`, if any.
    ///
    /// # Errors
    ///
    /// See [`Registry::load_at`].
    pub fn current(git: &Git) -> Result<Option<Self>, String> {
        match git
            .ref_target(REGISTRY_REF)
            .map_err(|error| error.message())?
        {
            Some(commit) => Self::load_at(git, &commit).map(Some),
            None => Ok(None),
        }
    }

    fn validate(&self) -> Result<(), String> {
        let mut seen: HashMap<(String, String), (KeyClass, String)> = HashMap::new();
        for class in KeyClass::ALL {
            for (number, line) in self.files[&class].lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let words: Vec<&str> = line.split_whitespace().collect();
                let where_ = format!("registry {} line {}", class.file(), number + 1);
                let [principal, key_type, blob, ..] = words.as_slice() else {
                    return Err(format!("{where_}: expected `<principal> <key type> <key>`"));
                };
                if !KEY_TYPES.contains(key_type) {
                    return Err(format!(
                        "{where_}: options and cert-authority lines are refused; expected a key type after the principal"
                    ));
                }
                if principal.contains(',') || principal.contains('*') || principal.contains('"') {
                    return Err(format!("{where_}: exactly one principal per line"));
                }
                let key = ((*key_type).to_owned(), (*blob).to_owned());
                if let Some((other_class, other_principal)) = seen.get(&key) {
                    return Err(format!(
                        "{where_}: this key is already in {} under {other_principal}; a key belongs to one class and one principal",
                        other_class.file()
                    ));
                }
                seen.insert(key, (class, (*principal).to_owned()));
            }
        }
        Ok(())
    }

    /// Writes the class file to a path Git and ssh-keygen can read, and returns it.
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn class_file(&self, git: &Git, class: KeyClass) -> Result<PathBuf, String> {
        let common = git
            .run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .map_err(|error| error.message())?;
        let dir = PathBuf::from(common)
            .join("bay")
            .join("registry")
            .join(&self.commit);
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let path = dir.join(class.file());
        if !path.exists() {
            let tmp = dir.join(format!(".{}.tmp", class.file()));
            std::fs::write(&tmp, &self.files[&class]).map_err(|error| error.to_string())?;
            std::fs::rename(&tmp, &path).map_err(|error| error.to_string())?;
        }
        Ok(path)
    }

    /// The registry version `named`, when it is on this registry's chain: this version or one of
    /// its ancestors (§ 10a). `None` for any other id, a commit this repository lacks, or one
    /// off the chain, so a forged registry commit is never a version anything is judged under.
    ///
    /// # Errors
    ///
    /// A git fault, or a version on the chain that does not load.
    pub fn version_on_chain(&self, git: &Git, named: &str) -> Result<Option<Self>, String> {
        let hex = named.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !hex || !matches!(named.len(), 40 | 64) {
            return Ok(None);
        }
        if named == self.commit {
            return Ok(Some(self.clone()));
        }
        let known = git
            .run_status(&["cat-file", "-e", &format!("{named}^{{commit}}")])
            .map_err(|error| error.message())?
            .0
            == 0;
        if !known {
            return Ok(None);
        }
        let (code, _, stderr) = git
            .run_status(&["merge-base", "--is-ancestor", named, &self.commit])
            .map_err(|error| error.message())?;
        match code {
            0 => Self::load_at(git, named).map(Some),
            1 => Ok(None),
            _ => Err(format!("merge-base --is-ancestor: {stderr}")),
        }
    }

    /// Whether `class`'s file enrols `principal` with this public key. Keys are compared by
    /// key type and key blob, never by line (§ 10a).
    #[must_use]
    pub fn holds(&self, class: KeyClass, principal: &str, key_type: &str, blob: &str) -> bool {
        self.files[&class].lines().any(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            matches!(words.as_slice(), [name, kind, key, ..] if *name == principal && *kind == key_type && *key == blob)
        })
    }
}

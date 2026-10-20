//! The key registry (`DESIGN` § 10a, Registry encoding).
//!
//! `refs/bay/registry` is an append-only chain whose tree holds one OpenSSH `allowed_signers`
//! file per key class: `agent`, `executor`, `producer`, `reviewer`, `registry`. The class is
//! the file: a verification names the file of the class it requires, so a key only in the
//! agent file never verifies as an executor, and a `review` record verifies only against the
//! reviewer file (ruled 2026-10-19, rulings-open-questions.md Q12). A public key appears in
//! exactly one file, each line has exactly one principal, and options or `cert-authority`
//! lines are refused. A reviewer name is neither a producer name (both write
//! `refs/notes/bay/evidence/<name>`) nor an agent name (an author never reviews its own
//! landing under one name, Q7). Enrolment is plain Git: whoever runs the repository commits
//! the files with a registry key and pushes.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::git::{Git, GitError};

pub const REGISTRY_REF: &str = crate::names::REGISTRY;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyClass {
    Agent,
    Executor,
    Producer,
    Reviewer,
    Registry,
}

impl KeyClass {
    pub const ALL: [Self; 5] = [
        Self::Agent,
        Self::Executor,
        Self::Producer,
        Self::Reviewer,
        Self::Registry,
    ];

    #[must_use]
    pub const fn file(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Executor => "executor",
            Self::Producer => "producer",
            Self::Reviewer => "reviewer",
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
            Self::Reviewer => "bay-reviewer",
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

/// The class files at `commit`, each empty when absent or unreadable, and whether the read is
/// complete: the tree was listed and every class file in it was read.
fn read_files(git: &Git, commit: &str) -> (Result<HashMap<KeyClass, String>, String>, bool) {
    let listed = git.run(&["ls-tree", "-z", "--name-only", commit]);
    let mut complete = listed.is_ok();
    let present: Vec<&str> = listed
        .as_deref()
        .map(|names| names.split('\0').collect())
        .unwrap_or_default();
    let mut files = HashMap::new();
    for class in KeyClass::ALL {
        // A file the listed tree does not hold is absent: no read to fail.
        if complete && !present.contains(&class.file()) {
            files.insert(class, String::new());
            continue;
        }
        let path = format!("{commit}:{}", class.file());
        let text = match git.run(&["cat-file", "blob", &path]) {
            Ok(text) => text,
            Err(GitError::Failed { .. }) => {
                complete = false;
                String::new()
            }
            Err(error) => return (Err(error.message()), false),
        };
        files.insert(class, text);
    }
    (Ok(files), complete)
}

impl Registry {
    /// The registry at `commit`, validated.
    ///
    /// # Errors
    ///
    /// A git fault, or a registry that breaks a line rule, puts one key in two places, or
    /// enrols a reviewer name as a producer or an agent too.
    pub fn load_at(git: &Git, commit: &str) -> Result<Self, String> {
        // The class files at a commit never change: read once per process (plan B2). Kept only
        // when the commit's tree was listed and every class file it lists was read: a failed
        // read of a listed file (a lazy fetch, a fault) may pass on a retry, and a commit whose
        // tree cannot be listed may be one not fetched yet.
        let files = crate::memo::at(
            "registry",
            git,
            commit,
            |(read, complete): &(Result<HashMap<KeyClass, String>, String>, bool)| {
                *complete && read.is_ok()
            },
            || read_files(git, commit),
        )
        .0?;
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
                // Q12 rule 4: a reviewer and a producer share the evidence-ref namespace. Q7: one
                // name as author and reviewer could review its own landing.
                if class == KeyClass::Reviewer {
                    for other in [KeyClass::Producer, KeyClass::Agent] {
                        if self.enrols(other, principal) {
                            return Err(format!(
                                "{where_}: reviewer {principal} is also enrolled in {}; a reviewer name is never a producer or an agent name",
                                other.file()
                            ));
                        }
                    }
                }
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
        let common = git.common_dir().map_err(|error| error.message())?;
        let dir = PathBuf::from(common)
            .join("bay")
            .join("registry")
            .join(&self.commit);
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let path = dir.join(class.file());
        if !path.exists() {
            let tmp = dir.join(format!(".{}.tmp", class.file()));
            std::fs::write(&tmp, &self.files[&class]).map_err(|error| error.to_string())?;
            crate::durable::rename(&tmp, &path).map_err(|error| error.to_string())?;
        }
        Ok(path)
    }

    /// The lines of class file `class` whose principal is `principal` itself, written next to
    /// [`Self::class_file`]: what `ssh-keygen -Y verify -I <principal>` checks a signature
    /// against when only a key enrolled under that very name may have made it. A principal is
    /// matched as a pattern by ssh-keygen, so the whole class file would let a line under another
    /// name whose pattern matches this one verify too.
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn principal_file(
        &self,
        git: &Git,
        class: KeyClass,
        principal: &str,
    ) -> Result<PathBuf, String> {
        let class_path = self.class_file(git, class)?;
        let mut lines = String::new();
        for line in self.files[&class]
            .lines()
            .map(str::trim)
            .filter(|line| line.split_whitespace().next() == Some(principal))
        {
            lines.push_str(line);
            lines.push('\n');
        }
        let name = format!(
            "{}.{}",
            class.file(),
            crate::agent_settings::hex(principal.as_bytes())
        );
        let path = class_path.with_file_name(&name);
        if !path.exists() {
            let tmp = class_path.with_file_name(format!(".{name}.tmp"));
            std::fs::write(&tmp, lines).map_err(|error| error.to_string())?;
            crate::durable::rename(&tmp, &path).map_err(|error| error.to_string())?;
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

    /// Whether `class`'s file enrols `principal` under any key: a registry actor of that class.
    #[must_use]
    pub fn enrols(&self, class: KeyClass, principal: &str) -> bool {
        self.files[&class].lines().any(|line| {
            let line = line.trim();
            !line.starts_with('#') && line.split_whitespace().next() == Some(principal)
        })
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

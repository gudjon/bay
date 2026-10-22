//! The key registry (`DESIGN` § 10a, Registry encoding).
//!
//! `refs/bay/registry` is an append-only chain whose tree holds one OpenSSH `allowed_signers`
//! file per key class: `agent`, `executor`, `producer`, `reviewer`, `approver`, `registry`. The
//! class is the file: a verification names the file of the class it requires, so a key only in
//! the agent file never verifies as an executor, a `review` record verifies only against the
//! reviewer file (ruled 2026-10-19, rulings-open-questions.md Q12), and an `approval` record only
//! against the approver file (G245). A public key appears in exactly one file, each line has
//! exactly one principal, and options or `cert-authority` lines are refused. A reviewer or an
//! approver name is neither a producer name (they all write `refs/notes/bay/evidence/<name>`)
//! nor an agent name (an author never reviews or approves its own landing under one name, Q7),
//! and no name is both a reviewer and an approver. Enrolment is `bay enrol` ([`crate::enrol`]):
//! it appends one key to its class file, checks the new files by these rules, signs the new
//! version with this machine's registry key and pushes it.
//!
//! A key leaves its class file through `bay enrol --remove` (owner ruling 2026-10-22), which
//! records the removal in the tree's `removed` file, one line each: `<class> <name> <reason>
//! <time> <key type> <key>`. The reason is `rotated` or `revoked`. A rotated key's records made
//! before its removal still verify, judged at their signing time ([`Registry::class_file_at`]);
//! a revoked key's records verify nowhere, also under an older version a merge was signed
//! under ([`Registry::without_revoked`]). The file only grows: the host refuses a version that
//! drops a line of it, and a removed key is never enrolled again.
//!
//! What a registry signs is the policy's to say (N-21, `[policy] signing` at the accepted base,
//! [`crate::policy::Signing`]): `all`, the default, signs every landing, request, chain and
//! record; `approvals` signs only `approval` records, and everything else runs as with no
//! registry. A registry read by [`Registry::current`] carries the mode of the trunk tip this
//! repository judges by; [`Registry::signs_all`] is the guard every other signing rule asks.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use crate::git::Git;
use crate::policy::Signing;

pub const REGISTRY_REF: &str = crate::names::REGISTRY;

/// The registry tree's file of removed keys.
pub const REMOVED_FILE: &str = "removed";

/// Why a key was removed (owner ruling 2026-10-22).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removed {
    /// Replaced in the ordinary course: its records made before the removal still verify.
    Rotated,
    /// Compromised: none of its records verifies, wherever it is read.
    Revoked,
}

impl Removed {
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Rotated => "rotated",
            Self::Revoked => "revoked",
        }
    }

    #[must_use]
    pub fn named(word: &str) -> Option<Self> {
        [Self::Rotated, Self::Revoked]
            .into_iter()
            .find(|reason| reason.word() == word)
    }
}

/// One line of the `removed` file.
#[derive(Clone, Debug)]
pub struct Removal {
    pub class: KeyClass,
    pub principal: String,
    pub reason: Removed,
    /// When it was removed, RFC 3339 UTC to the second.
    pub at: String,
    pub key_type: String,
    pub blob: String,
}

impl Removal {
    /// The line the `removed` file holds for it.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "{} {} {} {} {} {}",
            self.class.file(),
            self.principal,
            self.reason.word(),
            self.at,
            self.key_type,
            self.blob
        )
    }
}

/// The removals `text` holds, or the first line no removal can be.
fn parse_removed(text: &str) -> Result<Vec<Removal>, String> {
    let mut removals = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let where_ = format!("registry {REMOVED_FILE} line {}", number + 1);
        let words: Vec<&str> = line.split_whitespace().collect();
        let [class, principal, reason, at, key_type, blob] = words.as_slice() else {
            return Err(format!(
                "{where_}: expected `<class> <name> <rotated|revoked> <time> <key type> <key>`"
            ));
        };
        let class = KeyClass::named(class).ok_or_else(|| format!("{where_}: no class {class}"))?;
        let reason = Removed::named(reason)
            .ok_or_else(|| format!("{where_}: the reason is rotated or revoked, not {reason}"))?;
        if crate::clock::unix_of(at).is_none() {
            return Err(format!("{where_}: {at} is no RFC 3339 time"));
        }
        if !KEY_TYPES.contains(key_type) {
            return Err(format!("{where_}: {key_type} is no key type"));
        }
        removals.push(Removal {
            class,
            principal: (*principal).to_owned(),
            reason,
            at: (*at).to_owned(),
            key_type: (*key_type).to_owned(),
            blob: (*blob).to_owned(),
        });
    }
    Ok(removals)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyClass {
    Agent,
    Executor,
    Producer,
    Reviewer,
    /// A person whose signed `approval` record lets a change through an approval check (G245):
    /// it signs `approval` records only.
    Approver,
    Registry,
}

impl KeyClass {
    pub const ALL: [Self; 6] = [
        Self::Agent,
        Self::Executor,
        Self::Producer,
        Self::Reviewer,
        Self::Approver,
        Self::Registry,
    ];

    #[must_use]
    pub const fn file(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Executor => "executor",
            Self::Producer => "producer",
            Self::Reviewer => "reviewer",
            Self::Approver => "approver",
            Self::Registry => "registry",
        }
    }

    /// The class whose file is `name`, if any.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.file() == name)
    }

    /// The `ssh-keygen -Y` namespace for objects that are not Git objects.
    #[must_use]
    pub const fn namespace(self) -> &'static str {
        match self {
            Self::Agent => "bay-agent",
            Self::Executor => "bay-executor",
            Self::Producer => "bay-producer",
            Self::Reviewer => "bay-reviewer",
            Self::Approver => "bay-approver",
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

/// Whether `word` is a key type a registry line may carry.
#[must_use]
pub fn is_key_type(word: &str) -> bool {
    KEY_TYPES.contains(&word)
}

/// One registry version: the commit and its class files.
#[derive(Clone, Debug)]
pub struct Registry {
    pub commit: String,
    files: HashMap<KeyClass, String>,
    /// The `removed` file's lines (`bay enrol --remove`).
    removed: Vec<Removal>,
    /// What this registry signs (N-21): `all` from [`Registry::load_at`], the mode of the
    /// policy at the accepted base from [`Registry::current`].
    mode: Signing,
}

/// The class files at `commit`, each empty when absent, and whether the read is complete: the
/// tree was listed and every class file in it was read. G104b: a class file the tree names whose
/// blob cannot be read is an error naming it, never an empty class.
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
        let text = match git.file_at(commit, class.file()) {
            Ok(Some(text)) => text,
            // Only an unlisted tree reaches here without the file: absent, the read incomplete.
            Ok(None) => {
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
        let removed = match git.file_at(commit, REMOVED_FILE) {
            Ok(text) => parse_removed(&text.unwrap_or_default())?,
            Err(error) => return Err(error.message()),
        };
        let registry = Self {
            commit: commit.to_owned(),
            files,
            removed,
            mode: Signing::All,
        };
        registry.validate()?;
        Ok(registry)
    }

    /// The registry this repository holds at `refs/bay/registry`, if any, with the signing mode
    /// of the policy at the accepted base (N-21, [`accepted_base`]): a candidate never sets the
    /// mode it is judged by. With no base known the mode is `all`.
    ///
    /// # Errors
    ///
    /// See [`Registry::load_at`], or a policy at the base that cannot be read.
    pub fn current(git: &Git) -> Result<Option<Self>, String> {
        match git
            .ref_target(REGISTRY_REF)
            .map_err(|error| error.message())?
        {
            Some(commit) => {
                let mut registry = Self::load_at(git, &commit)?;
                if let Some(base) = accepted_base(git)? {
                    registry.mode = crate::policy::signing_at(git, &base)?;
                }
                Ok(Some(registry))
            }
            None => Ok(None),
        }
    }

    /// What this registry signs (N-21).
    #[must_use]
    pub const fn mode(&self) -> Signing {
        self.mode
    }

    /// Whether this registry signs everything (N-21, `signing = "all"`): landings, requests,
    /// chains, executor logs, reservations, live summaries, talks and every kind of evidence.
    /// Under `signing = "approvals"` only an `approval` record is signed, and every rule that
    /// asks this runs as with no registry.
    #[must_use]
    pub fn signs_all(&self) -> bool {
        self.mode == Signing::All
    }

    /// `registry` when it signs everything, else `None`: for a rule that requires a signature
    /// only when every write is signed (N-21).
    #[must_use]
    pub fn signing(registry: Option<&Self>) -> Option<&Self> {
        registry.filter(|registry| registry.signs_all())
    }

    /// The key of a judgement made under this registry: its commit, and the mode when it signs
    /// only approvals, so a judgement cached under one mode is never read under the other.
    #[must_use]
    pub fn judged_key(&self) -> String {
        match self.mode {
            Signing::All => self.commit.clone(),
            Signing::Approvals => format!("{}/{}", self.commit, Signing::Approvals.name()),
        }
    }

    /// The text of class file `class`, empty when the version holds none.
    #[must_use]
    pub fn text(&self, class: KeyClass) -> &str {
        &self.files[&class]
    }

    /// Checks the class files `file` gives, one per class, by the rules [`Registry::load_at`]
    /// applies, before they are committed (`bay enrol`, G299).
    ///
    /// # Errors
    ///
    /// The first rule the files break: a line rule, one key in two places, or a name enrolled
    /// in two classes that may not share it.
    pub fn check_files(file: impl Fn(KeyClass) -> String) -> Result<(), String> {
        let registry = Self {
            commit: String::new(),
            files: KeyClass::ALL
                .into_iter()
                .map(|class| (class, file(class)))
                .collect(),
            removed: Vec::new(),
            mode: Signing::All,
        };
        registry.validate()
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
                // G245: an approver writes `refs/notes/bay/evidence/<name>` like a producer or a
                // reviewer, and an agent that approved under its own name would let itself land.
                if class == KeyClass::Approver {
                    for other in [KeyClass::Producer, KeyClass::Reviewer, KeyClass::Agent] {
                        if self.enrols(other, principal) {
                            return Err(format!(
                                "{where_}: approver {principal} is also enrolled in {}; an approver name is never a producer, a reviewer or an agent name",
                                other.file()
                            ));
                        }
                    }
                }
            }
        }
        // A removed key is never enrolled again: a revoked one must stay out, and a rotated
        // one is replaced, not brought back.
        for removal in &self.removed {
            let key = (removal.key_type.clone(), removal.blob.clone());
            if let Some((class, principal)) = seen.get(&key) {
                return Err(format!(
                    "registry {REMOVED_FILE}: the key of {} {}, removed as {}, is enrolled again in {} under {principal}; a removed key is never enrolled again",
                    removal.class.file(),
                    removal.principal,
                    removal.reason.word(),
                    class.file()
                ));
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
        if git.is_ancestor(named, &self.commit)? {
            Self::load_at(git, named).map(Some)
        } else {
            Ok(None)
        }
    }

    /// The keys this version records as removed (`bay enrol --remove`).
    #[must_use]
    pub fn removed(&self) -> &[Removal] {
        &self.removed
    }

    /// The `removed` file this version holds, as its lines.
    #[must_use]
    pub fn removed_text(&self) -> String {
        self.removed
            .iter()
            .fold(String::new(), |mut text, removal| {
                let _ = writeln!(text, "{}", removal.line());
                text
            })
    }

    /// Class file `class` for a record signed at `at` (RFC 3339): the keys enrolled now, and the
    /// keys rotated out after `at`, which were enrolled when it was signed (owner ruling
    /// 2026-10-22). A revoked key is in neither. Written next to [`Self::class_file`].
    ///
    /// # Errors
    ///
    /// The file cannot be written.
    pub fn class_file_at(&self, git: &Git, class: KeyClass, at: &str) -> Result<PathBuf, String> {
        let class_path = self.class_file(git, class)?;
        let signed = crate::clock::unix_of(at);
        let rotated: Vec<&Removal> = self
            .removed
            .iter()
            .filter(|removal| {
                removal.class == class
                    && removal.reason == Removed::Rotated
                    && signed.is_some_and(|signed| {
                        crate::clock::unix_of(&removal.at).is_some_and(|removed| signed < removed)
                    })
            })
            .collect();
        if rotated.is_empty() {
            return Ok(class_path);
        }
        let mut text = self.files[&class].clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        for removal in &rotated {
            let _ = writeln!(
                text,
                "{} {} {}",
                removal.principal, removal.key_type, removal.blob
            );
        }
        let name = format!(
            "{}.at.{}",
            class.file(),
            crate::agent_settings::hex(at.as_bytes())
        );
        let path = class_path.with_file_name(&name);
        if !path.exists() {
            let tmp = class_path.with_file_name(format!(".{name}.tmp"));
            std::fs::write(&tmp, text).map_err(|error| error.to_string())?;
            crate::durable::rename(&tmp, &path).map_err(|error| error.to_string())?;
        }
        Ok(path)
    }

    /// This version without the keys `current` revokes: what a record signed under this older
    /// version is judged against now, so a revoked key verifies under no version (owner ruling
    /// 2026-10-22). This version itself when `current` revokes none of its keys.
    #[must_use]
    pub fn without_revoked(&self, current: &Self) -> Self {
        let revoked: Vec<&str> = current
            .removed
            .iter()
            .filter(|removal| removal.reason == Removed::Revoked)
            .map(|removal| removal.blob.as_str())
            .collect();
        let held = |line: &str| {
            line.split_whitespace()
                .nth(2)
                .is_some_and(|blob| revoked.contains(&blob))
        };
        if !KeyClass::ALL
            .iter()
            .any(|class| self.files[class].lines().any(held))
        {
            return self.clone();
        }
        let files = self
            .files
            .iter()
            .map(|(class, text)| {
                let kept = text.lines().filter(|line| !held(line)).fold(
                    String::new(),
                    |mut kept, line| {
                        let _ = writeln!(kept, "{line}");
                        kept
                    },
                );
                (*class, kept)
            })
            .collect();
        Self {
            commit: format!("{}-without-{}", self.commit, current.commit),
            files,
            removed: self.removed.clone(),
            mode: self.mode,
        }
    }

    /// The principals `class`'s file enrols, each once, in the file's order.
    #[must_use]
    pub fn principals(&self, class: KeyClass) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for line in self.files[&class].lines().map(str::trim) {
            if line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.split_whitespace().next()
                && !names.iter().any(|known| known == name)
            {
                names.push(name.to_owned());
            }
        }
        names
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

/// The trunk tip this repository judges by (N-21): in a work repository the tracking ref of the
/// remote trunk ([`crate::repo::Bootstrap`]), never a local branch a change can move; on a host
/// (a bare repository) its trunk branch, `bay.trunk` or `main` as the host hook names it, as it
/// stands before the push the hook judges. `None` when neither is known.
///
/// # Errors
///
/// A git fault.
fn accepted_base(git: &Git) -> Result<Option<String>, String> {
    if let Ok(bootstrap) = crate::repo::Bootstrap::read(git)
        && let Some(tip) = git
            .ref_target(&bootstrap.tracking_ref())
            .map_err(|error| error.message())?
    {
        return Ok(Some(tip));
    }
    let bare = git
        .run(&["rev-parse", "--is-bare-repository"])
        .map_err(|error| error.message())?;
    if bare.trim() != "true" {
        return Ok(None);
    }
    let trunk = git
        .run(&["config", "--get", "bay.trunk"])
        .unwrap_or_else(|_| "main".to_owned());
    git.ref_target(&format!("refs/heads/{}", trunk.trim()))
        .map_err(|error| error.message())
}

/// Whether trunk commit `commit` landed with every write signed (N-21): the policy at its first
/// parent, the base it was judged at, does not say `signing = "approvals"`. A parent or policy
/// that cannot be read is signed, so unknown never weakens a check. Asked only where a registry
/// exists: without one nothing is signed.
#[must_use]
pub fn landing_signed(git: &Git, commit: &str) -> bool {
    !matches!(
        crate::policy::signing_stated_at(git, &format!("{commit}^1")),
        Ok(Signing::Approvals)
    )
}

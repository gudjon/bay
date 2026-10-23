//! `bay enrol <class> <name> <public-key-file>` (G299; owner ruling 2026-10-21): adds one key to
//! the key registry ([`crate::registry`]), so a team never runs Git plumbing to set up keys.
//!
//! - It reads `refs/bay/registry` as the remote holds it, and refuses when the remote cannot be
//!   read: bay never guesses which registry it adds to.
//! - It appends `<name> <key type> <key>` to the class file and keeps every other file of the
//!   tree as it is. The new files are checked by the rules [`Registry::load_at`] applies before
//!   anything is written: a key already enrolled, or a name a rule refuses, is refused with the
//!   reason, and nothing is signed or pushed.
//! - The new version is signed with this machine's registry key, git config `bay.registryKey`
//!   (the path to its private half, as `bay.approverKey` is), its parent the remote's version.
//!   It is pushed as a fast-forward, never forced, and read back. The host's hook takes it only
//!   when the version before it enrols the key that signed it ([`crate::hook`]).
//! - The first enrolment creates the registry. It must be of class `registry`, with the public
//!   half of `bay.registryKey`, so the first version already enrols the key that signs the next.
//! - `bay enrol --remove <class> <name> --reason rotated|revoked` (owner ruling 2026-10-22) takes
//!   every key of `name` out of the class file and records each in the `removed` file with the
//!   reason and the time, in one version signed and pushed the same way. A rotated key's records
//!   made before then still verify; a revoked key's never do ([`crate::registry`]). The last
//!   registry key is never removed: nobody could sign the registry after it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::git::{Git, GitError};
use crate::registry::{KeyClass, REGISTRY_REF, REMOVED_FILE, Registry, Removal, Removed};
use crate::repo::Repo;
use crate::result::BayResult;
use crate::signing::Signer;

/// The git config key naming this machine's registry key: the path to its private half.
pub const REGISTRY_KEY: &str = "bay.registryKey";

/// What `bay enrol` asks.
pub struct Enrol {
    /// The class file the key goes in: `agent`, `executor`, `producer`, `reviewer`,
    /// `approver` or `registry`.
    pub class: String,
    /// The principal the key is enrolled under.
    pub name: String,
    /// The OpenSSH public key file (`<key>.pub`), relative to the directory bay runs in; none
    /// for a removal.
    pub public_key: Option<PathBuf>,
    /// `--remove --reason <reason>`: take the name's keys out instead.
    pub remove: Option<Removed>,
}

/// One public key: its key type and key blob, as a registry line holds them.
type PublicKey = (String, String);

/// `bay enrol <class> <name> <public-key-file>`.
#[must_use]
pub fn enrol(dir: &Path, request: &Enrol) -> BayResult {
    let repo = match Repo::open_fetched(dir) {
        Ok(repo) => repo,
        Err(error) => return error.into_result(),
    };
    let mut result = enrol_in(dir, &repo, request).unwrap_or_else(BayResult::failed);
    result.freshness = result.freshness.or_else(|| repo.freshness());
    result
}

fn enrol_in(dir: &Path, repo: &Repo, request: &Enrol) -> Result<BayResult, String> {
    let class = KeyClass::named(&request.class).ok_or_else(|| {
        let classes: Vec<&str> = KeyClass::ALL.into_iter().map(KeyClass::file).collect();
        format!(
            "{} is no key class. Name one of {}.",
            request.class,
            classes.join(", ")
        )
    })?;
    let name = request.name.as_str();
    principal(class, name)?;
    if let Some(reason) = request.remove {
        return remove(repo, class, name, reason);
    }
    let public_key = request
        .public_key
        .as_ref()
        .ok_or("Name the public key file to enrol: bay enrol <class> <name> <key.pub>.")?;
    let public = read_public(&dir.join(public_key))?;
    let (signing_key, signer_public) = registry_key(&repo.git)?;
    let parent = remote_registry(repo)?;
    let current = parent
        .as_deref()
        .map(|commit| Registry::load_at(&repo.git, commit))
        .transpose()?;
    let registrar = match &current {
        Some(registry) => registrar(registry, &signer_public)?,
        None if class != KeyClass::Registry => {
            return Err(
                "There is no registry yet, and its first key must be a registry key. Enrol a registry key first: bay enrol registry <name> <key.pub>, with the public half of bay.registryKey."
                    .to_owned(),
            );
        }
        None if public != signer_public => {
            let key = signing_key.display();
            return Err(format!(
                "The first registry key must be the one bay.registryKey signs with ({key}). Enrol a registry key first: bay enrol registry <name> {key}.pub"
            ));
        }
        None => name.to_owned(),
    };
    // A removed key is never enrolled again (0.3.3): refused here, before anything is signed,
    // whatever the host checks.
    if let Some(removal) = current
        .as_ref()
        .and_then(|registry| registry.removal_of(&public.0, &public.1))
    {
        return Err(format!(
            "Nothing was enrolled: that key was removed from {} {} as {} at {}, and a removed key is never enrolled again. Enrol a new key.",
            removal.class.file(),
            removal.principal,
            removal.reason.word(),
            removal.at
        ));
    }
    let text = class_text(current.as_ref(), class, name, &public);
    Registry::check_files(|other| {
        if other == class {
            text.clone()
        } else {
            current
                .as_ref()
                .map_or_else(String::new, |registry| registry.text(other).to_owned())
        }
    })
    .map_err(|why| format!("Nothing was enrolled: {why}."))?;
    let tree = new_tree(&repo.git, parent.as_deref(), &[(class.file(), &text)])?;
    let signer = Signer {
        key: signing_key,
        name: registrar,
    };
    let message = format!("enrol {} {name}", class.file());
    let commit = signed_commit(&repo.git, &tree, parent.as_deref(), &signer, &message)?;
    // Every rule a version is loaded by, here first: nothing is pushed that no bay could read.
    Registry::load_at(&repo.git, &commit).map_err(|why| format!("Nothing was enrolled: {why}."))?;
    push(repo, &commit)?;
    Ok(BayResult::ok(json!({
        "class": class.file(),
        "name": name,
        "registry": commit,
    })))
}

/// `bay enrol --remove`: every key of `name` out of class file `class`, each recorded in the
/// `removed` file with `reason` and now, in one version signed with this machine's registry key.
fn remove(repo: &Repo, class: KeyClass, name: &str, reason: Removed) -> Result<BayResult, String> {
    let (signing_key, signer_public) = registry_key(&repo.git)?;
    let parent =
        remote_registry(repo)?.ok_or("There is no registry yet, so there is no key to remove.")?;
    // A version that enrolled a removed key again (0.3.2) is repaired from: every other rule
    // holds, and the new version must pass them all.
    let current = Registry::load_for_repair(&repo.git, &parent)?;
    let registrar = registrar(&current, &signer_public)?;
    let at = crate::clock::rfc3339_of(crate::live::unix_now());
    let mut kept = String::new();
    let mut removals = Vec::new();
    for line in current.text(class).lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            [principal, key_type, blob, ..] if *principal == name => removals.push(Removal {
                class,
                principal: name.to_owned(),
                reason,
                at: at.clone(),
                key_type: (*key_type).to_owned(),
                blob: (*blob).to_owned(),
            }),
            _ => {
                kept.push_str(line);
                kept.push('\n');
            }
        }
    }
    if removals.is_empty() {
        return Err(format!(
            "{name} is not enrolled as {} {}, so nothing was removed.",
            article(class),
            class.file()
        ));
    }
    if class == KeyClass::Registry && kept.lines().all(|line| line.trim().is_empty()) {
        return Err(
            "That is the last registry key: without it nobody could sign the registry again, so nothing was removed. Enrol another registry key first."
                .to_owned(),
        );
    }
    let mut removed = current.removed_text();
    for removal in &removals {
        removed.push_str(&removal.line());
        removed.push('\n');
    }
    let tree = new_tree(
        &repo.git,
        Some(&parent),
        &[(class.file(), &kept), (REMOVED_FILE, &removed)],
    )?;
    let signer = Signer {
        key: signing_key,
        name: registrar,
    };
    let message = format!("remove {} {name} ({})", class.file(), reason.word());
    let commit = signed_commit(&repo.git, &tree, Some(&parent), &signer, &message)?;
    // The rules the host applies, here first: nothing is pushed that it would refuse.
    Registry::load_at(&repo.git, &commit).map_err(|why| format!("Nothing was removed: {why}."))?;
    push(repo, &commit)?;
    Ok(BayResult::ok(json!({
        "class": class.file(),
        "name": name,
        "removed": reason.word(),
        "keys": removals.len(),
        "at": at,
        "registry": commit,
    })))
}

/// "a" or "an" before the class's name.
fn article(class: KeyClass) -> &'static str {
    if matches!(
        class,
        KeyClass::Agent | KeyClass::Runner | KeyClass::Approver
    ) {
        "an"
    } else {
        "a"
    }
}

/// Refuses a name no registry line can hold, and, for a class whose name is also an evidence
/// writer's or an executor's, one that is not a short lower-case word.
fn principal(class: KeyClass, name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.starts_with('#')
        || name.contains(char::is_whitespace)
        || name.contains([',', '*', '"'])
    {
        return Err(format!(
            "The name {name:?} cannot be enrolled. A name is one word, with no , * or \" and no leading #."
        ));
    }
    if matches!(
        class,
        KeyClass::Runner | KeyClass::Producer | KeyClass::Reviewer | KeyClass::Approver
    ) {
        crate::names::operator_name(name).map_err(|why| {
            format!(
                "The {} name is a short lower-case word: {why}.",
                class.file()
            )
        })?;
    }
    Ok(())
}

/// The key type and key blob of the OpenSSH public key file at `path`.
fn read_public(path: &Path) -> Result<PublicKey, String> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        format!(
            "The public key file {} cannot be read: {error}.",
            path.display()
        )
    })?;
    let mut words = text.split_whitespace();
    if let (Some(kind), Some(blob)) = (words.next(), words.next())
        && crate::registry::is_key_type(kind)
    {
        return Ok((kind.to_owned(), blob.to_owned()));
    }
    Err(format!(
        "{} is not an OpenSSH public key. Name the .pub file, which reads `<key type> <key> [comment]`.",
        path.display()
    ))
}

/// This machine's registry key, `bay.registryKey`, and its public half: derived from the
/// private key, or read from `<key>.pub` beside it when ssh-keygen cannot derive it.
fn registry_key(git: &Git) -> Result<(PathBuf, PublicKey), String> {
    let key = git
        .config_get(REGISTRY_KEY)
        .ok()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "This machine has no registry key, so it cannot sign the registry. Set one config line: git config {REGISTRY_KEY} <path to the private key>, then bay enrol again."
            )
        })?;
    let key = PathBuf::from(key.trim());
    let public = crate::signing::public_key(&key).or_else(|why| {
        let mut beside = key.as_os_str().to_owned();
        beside.push(".pub");
        read_public(Path::new(&beside)).map_err(|_| {
            format!(
                "The registry key {REGISTRY_KEY} ({}) cannot be read: {why}.",
                key.display()
            )
        })
    })?;
    Ok((key, public))
}

/// The commit `refs/bay/registry` names in `ls-remote` output, if any.
fn remote_tip(listed: &str) -> Option<String> {
    listed.lines().find_map(|line| {
        let (commit, name) = line.split_once(char::is_whitespace)?;
        (name.trim() == REGISTRY_REF).then(|| commit.to_owned())
    })
}

/// The registry version the remote holds, fetched here; `None` when the remote has none. A
/// remote that cannot be read is an error: nothing is enrolled on a guess.
fn remote_registry(repo: &Repo) -> Result<Option<String>, String> {
    let unread = |error: GitError| {
        format!(
            "The registry on {} cannot be read, so nothing was enrolled. Try again once the remote answers: {}",
            repo.remote(),
            error.message()
        )
    };
    let listed = repo
        .git
        .run(&["ls-remote", repo.remote(), REGISTRY_REF])
        .map_err(unread)?;
    let Some(tip) = remote_tip(&listed) else {
        return Ok(None);
    };
    repo.git
        .run(&[
            "fetch",
            "-q",
            "--no-tags",
            repo.remote(),
            &format!("+{REGISTRY_REF}:{REGISTRY_REF}"),
        ])
        .map_err(unread)?;
    Ok(Some(tip))
}

/// The key that signs: the name `registry`'s registry file enrols this machine's registry key
/// under. A key the file does not hold is refused here, as the host would refuse the push.
fn registrar(registry: &Registry, key: &PublicKey) -> Result<String, String> {
    registry
        .principals(KeyClass::Registry)
        .into_iter()
        .find(|name| registry.holds(KeyClass::Registry, name, &key.0, &key.1))
        .ok_or_else(|| {
            format!(
                "This machine's registry key is not enrolled as a registry key, so the host would refuse the enrolment. Set {REGISTRY_KEY} to an enrolled registry key, or ask whoever holds one to run bay enrol."
            )
        })
}

/// Class file `class` of `current` (empty with no registry), with the line enrolling `public`
/// under `name` appended.
fn class_text(
    current: Option<&Registry>,
    class: KeyClass,
    name: &str,
    public: &PublicKey,
) -> String {
    let mut text = current.map_or_else(String::new, |registry| registry.text(class).to_owned());
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    let _ = writeln!(text, "{name} {} {}", public.0, public.1);
    text
}

/// The tree of `parent` (none: empty) with each named file of `files` holding its text; every
/// other entry is kept as it is.
fn new_tree(git: &Git, parent: Option<&str>, files: &[(&str, &str)]) -> Result<String, String> {
    let fault = |error: GitError| error.message();
    let listed = match parent {
        Some(parent) => git.run(&["ls-tree", "-z", parent]).map_err(fault)?,
        None => String::new(),
    };
    let mut names: Vec<String> = files.iter().map(|(name, _)| format!("\t{name}")).collect();
    // The runner class's file replaces the one a registry stored before 0.4.0, whose lines the
    // new text already holds (`Registry::text`).
    if files
        .iter()
        .any(|(name, _)| *name == KeyClass::Runner.file())
    {
        names.push(format!("\t{}", crate::registry::OLD_RUNNER_FILE));
    }
    let mut entries: Vec<String> = listed
        .split('\0')
        .filter(|entry| !entry.is_empty() && !names.iter().any(|own| entry.ends_with(own)))
        .map(str::to_owned)
        .collect();
    for (name, text) in files {
        let blob = git
            .run_with(
                &["hash-object", "-w", "--stdin"],
                &[],
                Some(text.as_bytes()),
            )
            .map_err(fault)?;
        entries.push(format!("100644 blob {blob}\t{name}"));
    }
    let mut input = entries.join("\0");
    input.push('\0');
    git.run_with(&["mktree", "-z"], &[], Some(input.as_bytes()))
        .map_err(fault)
}

/// The registry version of `tree` on `parent`, signed by `signer`'s key under its name.
fn signed_commit(
    git: &Git,
    tree: &str,
    parent: Option<&str>,
    signer: &Signer,
    message: &str,
) -> Result<String, String> {
    let mut args = signer.config_args();
    args.extend(["commit-tree".to_owned(), tree.to_owned(), "-S".to_owned()]);
    if let Some(parent) = parent {
        args.extend(["-p".to_owned(), parent.to_owned()]);
    }
    args.extend(["-m".to_owned(), message.to_owned()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let identity = signer.identity();
    let envs: Vec<(&str, &str)> = identity
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    git.run_with(&args, &envs, None).map_err(|error| {
        format!(
            "The registry key could not sign the enrolment, so nothing was enrolled: {}",
            error.message()
        )
    })
}

/// Pushes `commit` to the remote's `refs/bay/registry` as a fast-forward, never forced, reads
/// it back, then moves this repository's copy to it.
fn push(repo: &Repo, commit: &str) -> Result<(), String> {
    let git = &repo.git;
    git.run(&[
        "push",
        "-q",
        repo.remote(),
        &format!("{commit}:{REGISTRY_REF}"),
    ])
    .map_err(|error| {
        format!(
            "The remote refused the enrolment, so nothing changed there. Its reason: {}",
            error.message()
        )
    })?;
    let listed = git
        .run(&["ls-remote", repo.remote(), REGISTRY_REF])
        .map_err(|error| {
            format!(
                "The enrolment was sent, but the registry on {} could not be read back. Run bay doctor: {}",
                repo.remote(),
                error.message()
            )
        })?;
    let back = remote_tip(&listed);
    if back.as_deref() != Some(commit) {
        return Err(format!(
            "The enrolment was sent, but the registry on {} reads {} instead. Run bay enrol again.",
            repo.remote(),
            back.unwrap_or_else(|| "nothing".to_owned())
        ));
    }
    git.run(&["update-ref", REGISTRY_REF, commit])
        .map_err(|error| error.message())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{KeyClass, principal, remote_tip};

    #[test]
    fn the_remote_tip_is_read_from_its_exact_name_only() {
        let listed = "abc1\trefs/bay/registry-old\nabc2\trefs/bay/registry\n";
        assert_eq!(remote_tip(listed).as_deref(), Some("abc2"));
        assert_eq!(remote_tip(""), None);
    }

    #[test]
    fn a_name_is_one_word_and_an_approver_name_a_short_lower_case_one() {
        assert!(principal(KeyClass::Approver, "anna").is_ok());
        assert!(principal(KeyClass::Approver, "Anna").is_err());
        assert!(principal(KeyClass::Agent, "two words").is_err());
        assert!(principal(KeyClass::Registry, "#admin").is_err());
        assert!(principal(KeyClass::Registry, "a,b").is_err());
    }
}

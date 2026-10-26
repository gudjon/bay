//! `bay install --approver <name>` (owner ruling 2026-10-25: "bay setup for a non-technical
//! person must be ONE command"): this person, on their own machine and in the project's clone,
//! becomes an approver in one run, and is told in one plain sentence.
//!
//! It makes the registry key and the approver key in `~/.ssh` when this machine has none (no
//! passphrase, no prompt, mode 0600), and keeps any key it finds: it never overwrites one. It
//! sets `bay.registryKey`, `bay.approverKey` and `bay.approverName` in this clone. When the
//! remote has no registry yet, it starts one with this person's registry key, but only under
//! `[policy] signing = "approvals"` on main: a registry under `signing = "all"` makes every
//! landing need a runner key, so that is set up by hand. It then enrols the approver key, unless
//! the registry already holds it. Each enrolment is pushed and read back from the remote
//! (`crate::enrol`), and the end reads the remote's registry once more and finds the approver
//! there, or says what went wrong and the one thing to do. A second run changes nothing.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::enrol::{Enrol, REGISTRY_KEY};
use crate::registry::{KeyClass, REGISTRY_REF, Registry};
use crate::repo::Repo;
use crate::result::{BayResult, Decision};

/// The git config key naming this machine's approver key.
const APPROVER_KEY: &str = "bay.approverKey";
/// The git config key naming the approver this machine signs for.
const APPROVER_NAME: &str = "bay.approverName";

/// `bay install --approver <name>`.
#[must_use]
pub fn approver(dir: &Path, name: &str) -> BayResult {
    let mut changed = Vec::new();
    match run(dir, name, &mut changed) {
        Ok(result) => result,
        Err(why) if changed.is_empty() => BayResult::failed(why),
        // G389: a run that wrote keys or settings before it stopped says so, never "Nothing
        // was changed"; a second run keeps them and goes on.
        Err(why) => {
            BayResult::new(Decision::Failed).with_detail(json!({"why": why, "changed": changed}))
        }
    }
}

fn run(dir: &Path, name: &str, changed: &mut Vec<String>) -> Result<BayResult, String> {
    if name.is_empty()
        || !name.starts_with(|c: char| c.is_ascii_lowercase())
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "{name:?} is not a name bay can use. Use one short lower-case word, such as your first name."
        ));
    }
    let repo = Repo::open_fetched(dir).map_err(|error| error.message())?;
    let home = PathBuf::from(
        crate::platform::home()
            .ok_or("Your home folder is not set, so bay cannot keep your keys.")?,
    );
    let ssh = home.join(".ssh");
    let registry_key = key_path(&repo, REGISTRY_KEY, &ssh.join("bay-registry"));
    let approver_key = key_path(
        &repo,
        APPROVER_KEY,
        &ssh.join(format!("bay-approver-{name}")),
    );
    // G389: a project that cannot take this setup is told so before anything is written.
    repo.fetch_registry();
    let start_registry = Registry::current(&repo.git)?.is_none();
    if start_registry {
        let trunk = repo
            .git
            .run(&["rev-parse", "--verify", &repo.trunk_tracking_ref()])
            .map_err(|_| "bay cannot read main yet. Run bay status once, then this again.")?;
        let signing = crate::policy::signing_at(&repo.git, &trunk)?;
        if signing != crate::policy::Signing::Approvals {
            return Err(
                "This project has no key registry yet, and its rules sign every change (signing = \"all\"), which also needs runner keys. Ask whoever runs the project to add [policy] signing = \"approvals\" to .bay/policy.toml, then run this again."
                    .to_owned(),
            );
        }
    }
    let made_registry = ensure_key(&registry_key, "bay registry")?;
    if made_registry {
        changed.push(registry_key.display().to_string());
    }
    let made_approver = ensure_key(&approver_key, &format!("bay approver {name}"))?;
    if made_approver {
        changed.push(approver_key.display().to_string());
    }
    for (key, value) in [
        (REGISTRY_KEY, registry_key.display().to_string()),
        (APPROVER_KEY, approver_key.display().to_string()),
        (APPROVER_NAME, name.to_owned()),
    ] {
        if repo.git.run(&["config", "--get", key]).ok().as_deref() == Some(value.as_str()) {
            continue;
        }
        repo.git.run(&["config", key, &value]).map_err(|error| {
            format!(
                "bay could not save {key} in this project: {}",
                error.message()
            )
        })?;
        changed.push(key.to_owned());
    }

    let mut started = false;
    if start_registry {
        enrolled(dir, "registry", name, &registry_key)?;
        started = true;
    }
    let registry =
        Registry::current(&repo.git)?.ok_or("The registry did not arrive. Run this again.")?;
    let (key_type, blob) = public_key(&approver_key)?;
    let added = !registry.holds(KeyClass::Approver, name, &key_type, &blob);
    if added {
        enrolled(dir, "approver", name, &approver_key)?;
    }

    // The read-back: the remote's registry, as this clone now holds it, names the approver.
    let listed = repo
        .git
        .run(&["ls-remote", repo.remote(), REGISTRY_REF])
        .map_err(|error| format!("bay could not read the project's registry back from {}: {}. Check your connection and run this again.", repo.remote(), error.message()))?;
    let local = repo.git.run(&["rev-parse", "--verify", REGISTRY_REF]).ok();
    let remote = listed.split_whitespace().next().map(str::to_owned);
    let holds = Registry::current(&repo.git)?
        .is_some_and(|registry| registry.holds(KeyClass::Approver, name, &key_type, &blob));
    if remote.is_none() || remote != local || !holds {
        return Err(format!(
            "Your approval was not found on {} after it was sent. Run this again; if it says this twice, run bay doctor.",
            repo.remote()
        ));
    }
    Ok(BayResult::new(Decision::Ok).with_detail(json!({
        "approver": name,
        "registry_key": registry_key.display().to_string(),
        "approver_key": approver_key.display().to_string(),
        "made_keys": {"registry": made_registry, "approver": made_approver},
        "started_registry": started,
        "enrolled": added,
        "registry": local,
        "next": "bay waiting, then bay approve <change>",
    })))
}

/// The key `config` names in this clone, or `default`.
fn key_path(repo: &Repo, config: &str, default: &Path) -> PathBuf {
    repo.git
        .run(&["config", "--get", config])
        .ok()
        .filter(|path| !path.is_empty())
        .map_or_else(|| default.to_path_buf(), PathBuf::from)
}

/// Makes an ed25519 key at `path` when there is none, with no passphrase and mode 0600, and
/// keeps one that exists. Whether it made one.
fn ensure_key(path: &Path, label: &str) -> Result<bool, String> {
    let public = path.with_extension("pub");
    if path.exists() {
        if !public.exists() {
            return Err(format!(
                "{} exists but its .pub file is missing. bay never overwrites a key: move it aside, or put its .pub beside it, then run this again.",
                path.display()
            ));
        }
        return Ok(false);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("bay could not make {}: {error}", dir.display()))?;
        restrict(dir, 0o700);
    }
    let made = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", label, "-f"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("bay could not run ssh-keygen to make your key: {error}"))?;
    if !made.status.success() {
        return Err(format!(
            "ssh-keygen could not make {}: {}",
            path.display(),
            String::from_utf8_lossy(&made.stderr).trim()
        ));
    }
    restrict(path, 0o600);
    Ok(true)
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) {}

/// The key type and blob of `private`'s `.pub` file.
fn public_key(private: &Path) -> Result<(String, String), String> {
    let text = std::fs::read_to_string(private.with_extension("pub"))
        .map_err(|error| format!("bay could not read your public key: {error}"))?;
    let mut words = text.split_whitespace();
    match (words.next(), words.next()) {
        (Some(key_type), Some(blob)) => Ok((key_type.to_owned(), blob.to_owned())),
        _ => Err("Your public key file is not in OpenSSH form.".to_owned()),
    }
}

/// One `bay enrol <class> <name> <key>.pub`, as `crate::enrol` pushes and reads it back.
fn enrolled(dir: &Path, class: &str, name: &str, private: &Path) -> Result<(), String> {
    let result = crate::enrol::enrol(
        dir,
        &Enrol {
            class: class.to_owned(),
            name: name.to_owned(),
            public_key: Some(private.with_extension("pub")),
            remove: None,
        },
    );
    if result.decision == Decision::Ok {
        Ok(())
    } else {
        Err(result
            .detail
            .as_ref()
            .and_then(|detail| detail["why"].as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("bay could not enrol your {class} key.")))
    }
}

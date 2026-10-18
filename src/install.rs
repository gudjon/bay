//! `install`: the release's admin command (`DESIGN` § 15.1, § 15.3, § 15.4).
//!
//! Writes `$HOME/.local/bin/bay` and, when the Claude adapter is configured, bay's hook
//! entries in the project agent settings. Global Git config, user-global Claude settings,
//! `user.name`, `user.email` and other tools' hook entries stay unchanged.
//!
//! With a key registry, it also writes the repository-local signing identity (§ 15.3 item 3):
//! `gpg.format=ssh` and `gpg.ssh.allowedSignersFile` set to the class file of the key this box
//! uses. Keys and the registry themselves are enrolment, never install's job.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::agent_settings;
use crate::config::Config;
use crate::doctor::UNRELATED_HOOKS_KEY;
use crate::git::{Git, GitError};
use crate::registry::{KeyClass, Registry};
use crate::result::BayResult;
use crate::signing::Keys;

#[must_use]
pub fn install(dir: &Path) -> BayResult {
    match run(dir) {
        Ok(result) => result,
        Err(message) => BayResult::failed(message),
    }
}

fn run(dir: &Path) -> Result<BayResult, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set; install cannot name its path")?;
    let bin_dir = PathBuf::from(home).join(".local").join("bin");
    if !on_path(&bin_dir) {
        return Err(format!(
            "{} is not on PATH. Put that directory on PATH and run install again",
            bin_dir.display()
        ));
    }
    let target = bin_dir.join("bay");
    place_binary(&bin_dir, &target)?;

    let mut detail = json!({"binary": target.display().to_string()});

    let git = Git::new(dir);
    if let Ok(info) = git.repo_info()
        && !info.bare
    {
        let root = Path::new(&info.toplevel);
        let config = Config::load(root)?;
        if config.claude_adapter() {
            let path = agent_settings::claude_settings_path(root);
            let mut settings = agent_settings::read(&path)?;
            let before = agent_settings::unrelated_digest(&settings);
            agent_settings::add_claude_entries(&mut settings);
            let after = agent_settings::unrelated_digest(&settings);
            agent_settings::write(&path, &settings)?;
            git.run(&["config", "--local", UNRELATED_HOOKS_KEY, &after])
                .map_err(|error: GitError| error.message())?;
            detail["hooks"] = json!(path.display().to_string());
            detail["unrelated_hooks_unchanged"] = json!(before == after);
        }
        if let Some(allowed) = signing_identity(&git)? {
            detail["allowed_signers"] = json!(allowed);
        }
    }
    Ok(BayResult::ok(detail))
}

/// Sets this repository's local signing identity when it has a key registry: the class file
/// of the first key this box holds (agent, then executor, then producer).
fn signing_identity(git: &Git) -> Result<Option<String>, String> {
    let Some(registry) = Registry::current(git)? else {
        return Ok(None);
    };
    let keys = Keys::load(git);
    let class = if keys.agent.is_some() {
        KeyClass::Agent
    } else if keys.executor.is_some() {
        KeyClass::Executor
    } else if keys.producer.is_some() {
        KeyClass::Producer
    } else {
        return Ok(None);
    };
    let allowed = registry.class_file(git, class)?.display().to_string();
    for (key, value) in [
        ("gpg.format", "ssh"),
        ("gpg.ssh.allowedSignersFile", allowed.as_str()),
    ] {
        git.run(&["config", "--local", key, value])
            .map_err(|error| error.message())?;
    }
    Ok(Some(allowed))
}

fn on_path(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|entry| entry == dir))
}

fn place_binary(bin_dir: &Path, target: &Path) -> Result<(), String> {
    let source =
        std::env::current_exe().map_err(|error| format!("cannot find the running bay: {error}"))?;
    std::fs::create_dir_all(bin_dir)
        .map_err(|error| format!("cannot create {}: {error}", bin_dir.display()))?;
    if source.canonicalize().ok() == target.canonicalize().ok() {
        return Ok(());
    }
    let tmp = bin_dir.join(".bay.install-tmp");
    std::fs::copy(&source, &tmp)
        .map_err(|error| format!("cannot write {}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, target)
        .map_err(|error| format!("cannot place {}: {error}", target.display()))
}

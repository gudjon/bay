//! `install`: the release's admin command (`DESIGN` § 15.1, § 15.3, § 15.4).
//!
//! Writes `$HOME/.local/bin/bay`, the skill at `$HOME/.claude/skills/bay/` (or
//! `$CLAUDE_CONFIG_DIR/skills/bay/` when that is set) and in every other Claude config directory
//! on the machine (`skill::config_dirs`, G319) and, when the
//! Claude adapter is configured, bay's hook entries in the project agent settings (for the
//! Codex adapter, in `.codex/hooks.json`, G308). Global Git config, user-global Claude settings,
//! `user.name`, `user.email` and other tools' hook entries stay unchanged.
//!
//! With a key registry, it also writes the repository-local signing identity (§ 15.3 item 3):
//! `gpg.format=ssh` and `gpg.ssh.allowedSignersFile` set to the class file of the key this box
//! uses. Keys and the registry themselves are enrolment, never install's job. In a repository it
//! writes `bay.machine` when it is unset: the folded host name (`live.rs`), and the bootstrap
//! of the config (ruling: config location): `bay.main` and `bay.remote`, from `--trunk` and
//! `--remote`, else what they already hold, else the remote's default branch
//! (`refs/remotes/<remote>/HEAD`) and `origin`. It also sets the clone to keep a commit-graph
//! with changed-path Bloom filters on fetch, and writes the first one (G193, `commit_graph`).

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::agent_settings;
use crate::doctor::UNRELATED_HOOKS_KEY;
use crate::git::{Git, GitError};
use crate::registry::{KeyClass, Registry};
use crate::repo::{Bootstrap, DEFAULT_REMOTE, Fetch, OpenError, REMOTE_KEY, Repo, TRUNK_KEY};
use crate::result::BayResult;
use crate::signing::Keys;
use crate::skill;

/// With `machine`, only what belongs to the box (the binary and the skill): no repository's agent settings
/// or Git config, whatever directory it runs in. `bay update` installs this way.
#[must_use]
pub fn install(dir: &Path, machine: bool) -> BayResult {
    install_with(dir, machine, &Trunk::default())
}

/// The agent adapter whose hook entries install writes.
#[derive(Clone, Copy)]
enum Adapter {
    /// `.claude/settings.json`.
    Claude,
    /// `.codex/hooks.json` (G308).
    Codex,
}

/// The trunk and remote `bay install --main --remote` names for this repository.
#[derive(Default)]
pub struct Trunk {
    pub trunk: Option<String>,
    pub remote: Option<String>,
}

/// [`install`], writing the repository's bootstrap from `trunk`.
#[must_use]
pub fn install_with(dir: &Path, machine: bool, trunk: &Trunk) -> BayResult {
    match run(dir, machine, trunk) {
        Ok(result) => result,
        Err(message) => BayResult::failed(message),
    }
}

fn run(dir: &Path, machine: bool, trunk: &Trunk) -> Result<BayResult, String> {
    let home = PathBuf::from(
        crate::platform::home().ok_or("HOME is not set; install cannot name its path")?,
    );
    let bin_dir = home.join(".local").join("bin");
    if !on_path(&bin_dir) {
        return Err(format!(
            "{} is not on PATH. Put that directory on PATH and run install again",
            bin_dir.display()
        ));
    }
    // `bay.exe` on Windows (plan W2).
    let target = bin_dir.join(format!("bay{}", std::env::consts::EXE_SUFFIX));
    place_binary(&bin_dir, &target)?;

    // § 15.3 item 4: the skill this binary carries, at the caller's path and (G319) in every
    // other Claude config directory on the machine, the paths doctor checks.
    let git = Git::new(dir);
    let skill_dir = skill::installed_path(&home, skill::config_dir().as_deref());
    let written = skill::install_all(&skill::config_dirs(&home, &git))?;
    let written: Vec<String> = written
        .iter()
        .map(|path| path.display().to_string())
        .collect();

    let mut detail = json!({"binary": target.display().to_string(),
        "skill": skill_dir.display().to_string(), "skills": written});
    if machine {
        // G320: what this install writes, and what it never does, said on its result.
        detail["scope"] = json!("machine");
        detail["never"] = json!([
            "hook entries",
            "a repository's agent settings",
            "a repository's Git config"
        ]);
        return Ok(BayResult::ok(detail));
    }

    if let Ok(info) = git.repo_info()
        && !info.bare
    {
        let root = Path::new(&info.toplevel);
        detail["bootstrap"] = write_bootstrap(&git, trunk)?;
        // The adapter is the config's at the trunk tip, never the work tree's file.
        let adapter = match Repo::open_with(dir, Fetch::Never) {
            Ok(repo) if repo.config.claude_adapter() => Some(Adapter::Claude),
            Ok(repo) if repo.config.codex_adapter() => Some(Adapter::Codex),
            Ok(_) => None,
            Err(OpenError::ConfigMissing(_, missing)) => {
                detail["config"] = missing;
                None
            }
            Err(error) => return Err(error.message()),
        };
        if let Some(adapter) = adapter {
            let path = match adapter {
                Adapter::Claude => agent_settings::claude_settings_path(root),
                // G308: the project hooks file Codex reads.
                Adapter::Codex => agent_settings::codex_hooks_path(root),
            };
            let mut settings = agent_settings::read(&path)?;
            let before = agent_settings::unrelated_digest(&settings);
            match adapter {
                Adapter::Claude => agent_settings::add_claude_entries(&mut settings),
                Adapter::Codex => agent_settings::add_codex_entries(&mut settings),
            }
            let after = agent_settings::unrelated_digest(&settings);
            agent_settings::write(&path, &settings)?;
            git.run(&["config", "--local", UNRELATED_HOOKS_KEY, &after])
                .map_err(|error: GitError| error.message())?;
            detail["hooks"] = json!(path.display().to_string());
            detail["unrelated_hooks_unchanged"] = json!(before == after);
            // G320: entries that differ from the committed file reach no other clone until the
            // change that commits them lands; the result says so.
            let unlanded = git
                .run(&["status", "--porcelain", "--", &path.display().to_string()])
                .is_ok_and(|out| !out.trim().is_empty());
            if unlanded {
                detail["hooks_to_land"] = json!(true);
                detail["next"] = json!(format!(
                    "commit {} and merge it: every clone gets bay's hook entries from main",
                    path.display()
                ));
            }
        }
        // § 11a, ruling R1: the name this machine's live summary ref carries.
        detail["machine"] = json!(crate::live::ensure_machine(&git)?);
        // G193: this clone (an executor's, or the store its views share) keeps a commit-graph
        // with changed-path filters on every fetch.
        detail["commit_graph"] = crate::commit_graph::configure_clone(&git)?;
        if let Some(allowed) = signing_identity(&git)? {
            detail["allowed_signers"] = json!(allowed);
        }
    }
    Ok(BayResult::ok(detail))
}

/// Writes `bay.main` and `bay.remote` (repository-local): the names given, else what they
/// hold, else the remote's default branch and `origin`. With no trunk to write, writes nothing
/// and says so: bay's commands are `CONFIG_MISSING` until one is set.
fn write_bootstrap(git: &Git, trunk: &Trunk) -> Result<serde_json::Value, String> {
    let get = |key: &str| {
        git.run(&["config", "--local", "--get", key])
            .ok()
            .filter(|value| !value.is_empty())
    };
    let remote = trunk
        .remote
        .clone()
        .or_else(|| get(REMOTE_KEY))
        .unwrap_or_else(|| DEFAULT_REMOTE.to_owned());
    let name = trunk.trunk.clone().or_else(|| get(TRUNK_KEY)).or_else(|| {
        let head = git
            .run(&["symbolic-ref", "-q", &format!("refs/remotes/{remote}/HEAD")])
            .ok()?;
        head.strip_prefix(&format!("refs/remotes/{remote}/"))
            .map(str::to_owned)
    });
    let Some(name) = name else {
        return Ok(json!({
            "trunk": null,
            "remote": remote,
            "note": "no main named: run bay install --main <name>",
        }));
    };
    for (key, value) in [(REMOTE_KEY, remote.as_str()), (TRUNK_KEY, name.as_str())] {
        git.run(&["config", "--local", key, value])
            .map_err(|error| error.message())?;
    }
    let read = Bootstrap::of(git.dir()).and_then(Result::ok);
    Ok(json!({
        "trunk": read.as_ref().map(|bootstrap| bootstrap.trunk.clone()),
        "remote": read.map(|bootstrap| bootstrap.remote),
    }))
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
    // Another bay may be running from `target`: on Windows it moves aside (plan W3).
    crate::durable::replace_program(&tmp, target)
        .map_err(|error| format!("cannot place {}: {error}", target.display()))
}

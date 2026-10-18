//! `bay doctor`: the capability probe (`DESIGN` § 15.2, `INTERFACES` § 8).
//!
//! Doctor reads. It writes no settings file and no Git config.

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

use crate::agent_settings;
use crate::config::Config;
use crate::git::{self, Git, GitError, RepoInfo};
use crate::host;
use crate::result::{BayResult, Decision, Reason};
use crate::version::{
    CLAUDE_HOOK_MINIMUM, GIT_FLOOR_AGENT_VIEW, GIT_FLOOR_HOSTED_TRUNK, GIT_RECOMMENDED,
    GIT_SHARED_CACHE, Version,
};

/// Where `bay install` records the digest of the hook entries that are not bay's.
pub const UNRELATED_HOOKS_KEY: &str = "bay.unrelated-hooks-digest";

const CLAUDE_RECOVERY: &str = "upgrade Claude Code or run without the Claude adapter";

enum Claude {
    NotTheAgent,
    Absent,
    Found(Version),
}

#[derive(Default)]
struct Report {
    rows: Vec<Value>,
    first_red: Option<Reason>,
    /// The first red row that is not a version, as the doctor field names it.
    doctor_note: Option<&'static str>,
}

impl Report {
    fn red(&mut self, reason: Reason, note: Option<&'static str>) {
        self.first_red.get_or_insert(reason);
        if let Some(note) = note {
            self.doctor_note.get_or_insert(note);
        }
    }
}

#[must_use]
pub fn doctor(dir: &Path) -> BayResult {
    match probe(dir) {
        Ok(result) => result,
        Err(message) => BayResult::failed(message),
    }
}

fn probe(dir: &Path) -> Result<BayResult, String> {
    let git = Git::new(dir);
    let git_line = git.version().map_err(|error| error.message())?;
    let git_version = Version::parse(&git_line);
    let info = git.repo_info().map_err(|error| error.message())?;
    let config = if info.bare {
        Config::default()
    } else {
        Config::load(Path::new(&info.toplevel))?
    };

    let mut report = Report::default();

    let (role, floor) = if info.bare {
        ("bay-hosted-trunk", GIT_FLOOR_HOSTED_TRUNK)
    } else {
        ("agent-view", GIT_FLOOR_AGENT_VIEW)
    };
    let git_green = git_version >= Version::parse(floor);
    let mut git_row = json!({"name": "git", "role": role, "version": git_version.to_string(),
        "floor": floor, "state": state(git_green)});
    if !git_green {
        git_row["recovery"] = json!("upgrade git");
        report.red(Reason::GitBelowFloor, None);
    }
    report.rows.push(git_row);

    // Below 2.48 the shared cache is refused and bay uses a full clone per cache (§ 10).
    let cache_ok = git_version >= Version::parse(GIT_SHARED_CACHE);
    report.rows.push(if cache_ok {
        json!({"name": "shared-cache", "required": false, "floor": GIT_SHARED_CACHE, "state": "green"})
    } else {
        json!({"name": "shared-cache", "required": false, "floor": GIT_SHARED_CACHE,
            "state": "refused", "fallback": "full clone per cache"})
    });
    let features_ok = git_version >= Version::parse(GIT_RECOMMENDED);
    report
        .rows
        .push(json!({"name": "features", "required": false,
        "recommended": GIT_RECOMMENDED, "state": if features_ok { "green" } else { "degraded" }}));

    let claude = if config.claude_adapter() {
        probe_claude()?
    } else {
        Claude::NotTheAgent
    };
    let minimum = Version::parse(CLAUDE_HOOK_MINIMUM);
    let claude_row = match &claude {
        Claude::NotTheAgent => {
            json!({"name": "claude", "state": "green", "note": "not the agent on this box"})
        }
        Claude::Absent => {
            report.red(Reason::ClaudeAbsent, None);
            json!({"name": "claude", "state": "red", "floor": CLAUDE_HOOK_MINIMUM,
                "note": "claude is absent", "recovery": CLAUDE_RECOVERY})
        }
        Claude::Found(version) if *version < minimum => {
            report.red(Reason::ClaudeBelowFloor, None);
            json!({"name": "claude", "version": version.to_string(), "floor": CLAUDE_HOOK_MINIMUM,
                "state": "red", "recovery": CLAUDE_RECOVERY})
        }
        Claude::Found(version) => json!({"name": "claude", "version": version.to_string(),
            "floor": CLAUDE_HOOK_MINIMUM, "state": "green"}),
    };
    report.rows.push(claude_row);

    remote_row(&git, &config, info.bare, &mut report)?;
    hooks_row(&git, &config, &info, &mut report)?;

    let ready = report.first_red.is_none();
    let line = status_line(
        &git_version,
        floor,
        git_green,
        &claude,
        &minimum,
        ready,
        report.doctor_note,
    );
    let detail = json!({
        "bay": env!("CARGO_PKG_VERSION"),
        "git": git_line,
        "repository": info.toplevel,
        "object format": info.object_format,
        "ref format": info.ref_format,
        "branch": info.branch,
        "role": role,
        "rows": report.rows,
    });
    let mut result = match report.first_red {
        None => BayResult::new(Decision::Ok),
        Some(reason) => BayResult::blocked(reason),
    }
    .with_detail(detail);
    result.human = Some(line);
    Ok(result)
}

const fn state(green: bool) -> &'static str {
    if green { "green" } else { "red" }
}

fn remote_row(git: &Git, config: &Config, bare: bool, report: &mut Report) -> Result<(), String> {
    if bare {
        report.rows.push(
            json!({"name": "remote", "state": "not applicable", "note": "this box is the host"}),
        );
        return Ok(());
    }
    let remote = config.remote();
    let row = match git.remote_url(remote).map_err(|error| error.message())? {
        None => {
            report.red(Reason::RemoteUnreachable, Some("remote not configured"));
            json!({"name": "remote", "remote": remote, "state": "not configured"})
        }
        Some(_) if !git.remote_answers(remote) => {
            report.red(Reason::RemoteUnreachable, Some("remote not reachable"));
            json!({"name": "remote", "remote": remote, "state": "not reachable"})
        }
        Some(_) => json!({"name": "remote", "remote": remote, "state": "green"}),
    };
    report.rows.push(row);
    Ok(())
}

fn hooks_row(
    git: &Git,
    config: &Config,
    info: &RepoInfo,
    report: &mut Report,
) -> Result<(), String> {
    if info.bare {
        // § 10a, G15: a host without the trunk hook is refused.
        if host::has_trunk_hook(git) {
            report
                .rows
                .push(json!({"name": "trunk-hook", "state": "green"}));
        } else {
            report.red(Reason::HooksMissing, Some("host has no trunk hook"));
            report
                .rows
                .push(json!({"name": "trunk-hook", "state": "red",
                "recovery": "create the host with bay host init"}));
        }
        return Ok(());
    }
    if !config.claude_adapter() {
        report.rows.push(
            json!({"name": "hooks", "state": "green", "note": "no Claude adapter configured"}),
        );
        return Ok(());
    }
    let path = agent_settings::claude_settings_path(Path::new(&info.toplevel));
    let settings = agent_settings::read(&path)?;
    let missing = agent_settings::missing_claude_events(&settings);
    let mut row = json!({"name": "hooks", "state": state(missing.is_empty())});
    if !missing.is_empty() {
        row["missing"] = json!(missing);
        row["recovery"] = json!("run install again");
        report.red(Reason::HooksMissing, Some("hooks missing"));
    }
    if let Some(recorded) = recorded_unrelated_digest(git)? {
        row["unrelated_hooks_unchanged"] =
            json!(recorded == agent_settings::unrelated_digest(&settings));
    }
    report.rows.push(row);
    Ok(())
}

fn probe_claude() -> Result<Claude, String> {
    let mut cmd = Command::new("claude");
    cmd.arg("--version");
    match git::run_bounded(cmd, "claude --version") {
        Err(GitError::Spawn(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(Claude::Absent)
        }
        Err(error) => Err(error.message()),
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            Version::find(&text)
                .map(Claude::Found)
                .ok_or_else(|| format!("claude --version printed no version: {}", text.trim()))
        }
    }
}

fn recorded_unrelated_digest(git: &Git) -> Result<Option<String>, String> {
    match git.run(&["config", "--local", "--get", UNRELATED_HOOKS_KEY]) {
        Ok(value) => Ok(Some(value)),
        Err(GitError::Failed { .. }) => Ok(None),
        Err(error) => Err(error.message()),
    }
}

fn status_line(
    git_version: &Version,
    floor: &str,
    git_green: bool,
    claude: &Claude,
    minimum: &Version,
    ready: bool,
    doctor_note: Option<&str>,
) -> String {
    let git_field = match (ready, git_green) {
        (true, _) => format!("git {git_version}"),
        (false, true) => format!("git {git_version} (floor {floor})"),
        (false, false) => format!("git {git_version} (floor {floor}, upgrade git)"),
    };
    let claude_field = match claude {
        Claude::NotTheAgent => "claude not the agent on this box".to_owned(),
        Claude::Absent => format!("claude absent ({CLAUDE_RECOVERY})"),
        Claude::Found(version) if version < minimum => {
            format!("claude {version} (floor {CLAUDE_HOOK_MINIMUM}, {CLAUDE_RECOVERY})")
        }
        Claude::Found(version) => format!("claude {version}"),
    };
    let doctor_field = match (ready, doctor_note) {
        (true, _) => "doctor ready".to_owned(),
        (false, None) => "doctor not ready".to_owned(),
        (false, Some(note)) => format!("doctor not ready ({note})"),
    };
    format!(
        "bay {} | {git_field} | {claude_field} | {doctor_field}",
        env!("CARGO_PKG_VERSION")
    )
}

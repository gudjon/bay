//! `bay doctor`: the capability probe (`DESIGN` § 15.2, `INTERFACES` § 8), and the `bay`
//! row of `bay update` (installed version, channel, newest recorded release, pin).
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
use crate::skill;
use crate::version::{
    CLAUDE_HOOK_MINIMUM, GIT_FLOOR_AGENT_VIEW, GIT_FLOOR_ANY, GIT_FLOOR_HOSTED_TRUNK,
    GIT_RECOMMENDED, GIT_SHARED_CACHE, Version,
};

/// Where `bay install` records the digest of the hook entries that are not bay's.
pub const UNRELATED_HOOKS_KEY: &str = "bay.unrelated-hooks-digest";

const CLAUDE_RECOVERY: &str = "upgrade Claude Code or run without the Claude adapter";

enum Claude {
    NotConfigured,
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

/// The role of this box, which names its Git floor (`DESIGN` § 15.2, `GIT-FEATURES` § 1).
#[derive(Clone, Copy)]
enum Role {
    /// A repository that is not a bay-hosted trunk.
    AgentView,
    /// The bare repository `bay host init` creates (§ 10a).
    HostedTrunk,
    /// Doctor runs outside every repository: only the machine rows apply, at the floor of any
    /// machine running bay.
    NoRepository,
}

impl Role {
    /// The role name `DESIGN` § 15.2 gives. Outside a repository there is no role: `null`.
    const fn name(self) -> Option<&'static str> {
        match self {
            Self::AgentView => Some("agent-view"),
            Self::HostedTrunk => Some("bay-hosted-trunk"),
            Self::NoRepository => None,
        }
    }

    const fn floor(self) -> &'static str {
        match self {
            Self::AgentView => GIT_FLOOR_AGENT_VIEW,
            Self::HostedTrunk => GIT_FLOOR_HOSTED_TRUNK,
            Self::NoRepository => GIT_FLOOR_ANY,
        }
    }
}

fn probe(dir: &Path) -> Result<BayResult, String> {
    let git = Git::new(dir);
    let git_line = git.version().map_err(|error| error.message())?;
    let git_version = Version::parse(&git_line);
    // Outside a repository the machine rows still answer; the repository rows do not apply.
    let info = if git.outside_repository().map_err(|error| error.message())? {
        None
    } else {
        Some(git.repo_info().map_err(|error| error.message())?)
    };
    let config = match &info {
        Some(info) if !info.bare => Config::load(Path::new(&info.toplevel))?,
        _ => Config::default(),
    };

    let mut report = Report::default();
    report.rows.push(crate::update::doctor_row());

    let role = match &info {
        None => Role::NoRepository,
        Some(info) if info.bare => Role::HostedTrunk,
        Some(_) => Role::AgentView,
    };
    let floor = role.floor();
    let git_green = git_rows(&git_version, role, &mut report);
    let role = role.name();

    let claude = if config.claude_adapter() {
        probe_claude()?
    } else {
        Claude::NotConfigured
    };
    let minimum = Version::parse(CLAUDE_HOOK_MINIMUM);
    let claude_row = match &claude {
        Claude::NotConfigured => {
            json!({"name": "claude", "state": "green", "note": "Claude adapter not configured"})
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

    if let Some(info) = &info {
        remote_row(&git, &config, info.bare, &mut report)?;
        hooks_row(&git, &config, info, &mut report)?;
        quarantine_row(&git, &config, info.bare, &mut report);
        if !info.bare {
            live_row(&git, &mut report);
        }
    } else {
        let note = "not in a git repository";
        report
            .rows
            .push(json!({"name": "remote", "remote": config.remote(),
            "state": "not configured", "note": note}));
        report
            .rows
            .push(json!({"name": "hooks", "state": "not applicable", "note": note}));
    }

    skill_row(&config, &mut report)?;
    if info.is_none() {
        // § 15.2: with no configured remote the box is not ready, and outside a repository no
        // remote is configured. A red machine row names the reason first, since those rows are
        // what doctor outside a repository is run for (ruling: doctor outside a repository).
        report.red(Reason::RemoteUnreachable, Some("not in a git repository"));
    }

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
        "bay": crate::version::BAY,
        "git": git_line,
        "repository": info.as_ref().map(|info| &info.toplevel),
        "object format": info.as_ref().map(|info| &info.object_format),
        "ref format": info.as_ref().map(|info| &info.ref_format),
        "branch": info.as_ref().and_then(|info| info.branch.as_ref()),
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

/// The Git rows: the floor for `role`, the shared cache and the recommended features. Returns
/// whether the floor row is green.
fn git_rows(git_version: &Version, role: Role, report: &mut Report) -> bool {
    let floor = role.floor();
    let git_green = *git_version >= Version::parse(floor);
    let mut git_row = json!({"name": "git", "role": role.name(),
        "version": git_version.to_string(), "floor": floor, "state": state(git_green)});
    if !git_green {
        git_row["recovery"] = json!("upgrade git");
        report.red(Reason::GitBelowFloor, None);
    }
    report.rows.push(git_row);

    // Below 2.48 the shared cache is refused and bay uses a full clone per cache (§ 10).
    let cache_ok = *git_version >= Version::parse(GIT_SHARED_CACHE);
    report.rows.push(if cache_ok {
        json!({"name": "shared-cache", "required": false, "floor": GIT_SHARED_CACHE, "state": "green"})
    } else {
        json!({"name": "shared-cache", "required": false, "floor": GIT_SHARED_CACHE,
            "state": "refused", "fallback": "full clone per cache"})
    });
    let features_ok = *git_version >= Version::parse(GIT_RECOMMENDED);
    report
        .rows
        .push(json!({"name": "features", "required": false,
        "recommended": GIT_RECOMMENDED, "state": if features_ok { "green" } else { "degraded" }}));
    git_green
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

/// The open quarantines of the policy at trunk as this box last read it (`DESIGN` § 7: an
/// overdue quarantine is shown by `bay doctor`). Present only when a quarantine is open. It is
/// never red: `INTERFACES` § 8 has no reason code for it, and an overdue quarantine stays in
/// force until the fixing change removes it.
fn quarantine_row(git: &Git, config: &Config, bare: bool, report: &mut Report) {
    let trunk = if bare {
        let name = git
            .run(&["config", "--get", host::TRUNK_KEY])
            .unwrap_or_else(|_| config.trunk().to_owned());
        format!("refs/heads/{name}")
    } else {
        format!("refs/remotes/{}/{}", config.remote(), config.trunk())
    };
    let open = crate::status::open_quarantines(git, &trunk);
    if open.is_empty() {
        return;
    }
    let overdue = open.iter().any(|quarantine| quarantine["overdue"] == true);
    report.rows.push(json!({
        "name": "quarantine",
        "required": false,
        "state": if overdue { "overdue" } else { "green" },
        "quarantines": open,
    }));
}

/// The last live summary push of this repository (§ 11a), present only when it failed: the
/// pusher runs detached from the agent hook, so its refusal (a live ref bound to another key,
/// for one) is shown here instead of being lost. Never red: no reason code names it.
fn live_row(git: &Git, report: &mut Report) {
    let Ok(common) = git.run(&["rev-parse", "--path-format=absolute", "--git-common-dir"]) else {
        return;
    };
    let Some(last) = crate::live::last_push(Path::new(&common)) else {
        return;
    };
    if last["ok"] == false {
        report.rows.push(json!({
            "name": "live",
            "required": false,
            "state": "failed",
            "at": last["at"],
            "why": last["why"],
        }));
    }
}

/// The installed skill at `$HOME/.claude/skills/bay/` against the skill this bay version
/// installs (§ 15.2, G49). A consumer repository's skills are never this copy. A mismatch is
/// red on every box; a missing copy is red only where the Claude adapter is configured.
fn skill_row(config: &Config, report: &mut Report) -> Result<(), String> {
    let home =
        std::env::var_os("HOME").ok_or("HOME is not set; doctor cannot name the skill path")?;
    let path = skill::installed_path(Path::new(&home));
    let required = config.claude_adapter();
    let mut row =
        json!({"name": "skill", "path": path.display().to_string(), "required": required});
    match skill::compare(&path)? {
        skill::Installed::Matches => row["state"] = json!("green"),
        skill::Installed::Missing if !required => row["state"] = json!("missing"),
        installed => {
            let note = if installed == skill::Installed::Missing {
                "skill not installed"
            } else {
                "skill does not match this bay"
            };
            row["state"] = json!("red");
            row["note"] = json!(note);
            row["recovery"] = json!("run install again");
            report.red(Reason::SkillMismatch, Some(note));
        }
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
        Claude::NotConfigured => "claude adapter not configured".to_owned(),
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
        crate::version::BAY
    )
}

//! `bay doctor`: the capability probe (`DESIGN` § 15.2, `INTERFACES` § 8), and the `bay`
//! row of `bay update` (installed version, newest recorded release, pin).
//!
//! Doctor reads. It writes no settings file and no Git config.

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

use crate::agent_settings;
use crate::commit_graph::State as GraphState;
use crate::config::Config;
use crate::git::{self, Git, GitError, RepoInfo};
use crate::host;
use crate::repo::{Bootstrap, Fetch, OpenError, Repo};
use crate::result::{BayResult, Decision, Reason};
use crate::skill;
use crate::version::{
    CLAUDE_HOOK_MINIMUM, GIT_FLOOR_AGENT_VIEW, GIT_FLOOR_ANY, GIT_FLOOR_EXECUTOR,
    GIT_FLOOR_HOSTED_TRUNK, GIT_RECOMMENDED, GIT_SHARED_CACHE, Version,
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
    doctor_as(dir, false)
}

/// `bay doctor`, or with `executor` `bay doctor --role executor`: the box is judged as an
/// executor box whatever role doctor would infer (G63). The role is the operator's statement;
/// an unsigned executor box holds no key doctor could infer it from.
#[must_use]
pub fn doctor_as(dir: &Path, executor: bool) -> BayResult {
    match probe(dir, executor) {
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
    /// A box that runs `bay executor run`, as `--role executor` states (G63).
    Executor,
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
            Self::Executor => Some("executor"),
            Self::NoRepository => None,
        }
    }

    const fn floor(self) -> &'static str {
        match self {
            Self::AgentView => GIT_FLOOR_AGENT_VIEW,
            Self::HostedTrunk => GIT_FLOOR_HOSTED_TRUNK,
            Self::Executor => GIT_FLOOR_EXECUTOR,
            Self::NoRepository => GIT_FLOOR_ANY,
        }
    }
}

fn probe(dir: &Path, executor: bool) -> Result<BayResult, String> {
    let git = Git::new(dir);
    let git_line = git.version().map_err(|error| error.message())?;
    let git_version = Version::parse(&git_line);
    // Outside a repository the machine rows still answer; the repository rows do not apply.
    let info = if git.outside_repository().map_err(|error| error.message())? {
        None
    } else {
        Some(git.repo_info().map_err(|error| error.message())?)
    };
    // The config every command runs under: the one committed at the trunk tracking ref
    // (ruling: config location), read without a fetch, since doctor only reads.
    let (config, config_row) = match &info {
        Some(info) if !info.bare => repository_config(dir)?,
        _ => (Config::default(), None),
    };

    let mut report = Report::default();
    report.rows.push(crate::update::doctor_row());

    let role = match &info {
        _ if executor => Role::Executor,
        None => Role::NoRepository,
        Some(info) if info.bare => Role::HostedTrunk,
        Some(_) => Role::AgentView,
    };
    let floor = role.floor();
    let git_green = git_rows(&git_version, role, &mut report);
    let role = role.name();
    // Whether the role was stated with `--role` or inferred from the repository and keys: an
    // unsigned executor box is inferred as an agent view, and says so (G63).
    let role_source = role.map(|_| if executor { "stated" } else { "inferred" });
    let keys = crate::signing::Keys::load(&git);

    let claude = if config.claude_adapter() {
        probe_claude()?
    } else {
        Claude::NotConfigured
    };
    let minimum = Version::parse(CLAUDE_HOOK_MINIMUM);
    claude_row(&claude, &minimum, &mut report);

    if let Some(info) = &info {
        report.rows.extend(config_row);
        remote_row(&git, &config, info.bare, &mut report)?;
        hooks_row(&git, &config, info, &mut report)?;
        if info.bare {
            report.rows.push(ref_format_row(&info.ref_format));
        }
        report.rows.push(store_row(&git, &config, info));
        quarantine_row(&git, &config, info.bare, &mut report);
        if !info.bare {
            repository_row(&git, &config, &mut report);
            live_row(&git, &mut report);
            environments_row(&git, &config, executor, &mut report);
        }
        if executor && !info.bare {
            check_env_row(&git, &config, &mut report);
            executor_keys_row(&git, &keys, &mut report);
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
    custody_row(&git, info.as_ref(), &mut report);
    if info.is_none() {
        // § 15.2: with no configured remote the box is not ready, and outside a repository no
        // remote is configured. A red machine row names the reason first, since those rows are
        // what doctor outside a repository is run for (ruling: doctor outside a repository).
        report.red(Reason::RemoteUnreachable, Some("not in a git repository"));
    }
    // Ruling R14, on every platform. The row comes after the outside-repository red, so that
    // reason still comes first there; the choice it reads is the box's, in or out of a
    // repository.
    let runs_checks = executor || keys.executor.is_some() || keys.producer.is_some();
    sandbox_row(&crate::check::Sandbox::here(), runs_checks, &mut report);

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
        "role_source": role_source,
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

/// G172: the object store and its maintenance, as the site set them up. bay creates no store
/// and runs no maintenance, so every finding is advisory and names the site's own choice.
/// G192: it also reports the commit-graph (missing, without Bloom filters Git reads, or ok),
/// advisory too: a graph only speeds walks up.
fn store_row(git: &Git, config: &Config, info: &crate::git::RepoInfo) -> Value {
    let get = |key: &str| git.run(&["config", "--get", key]).ok();
    let remote = config.remote();
    let promisor = get(&format!("remote.{remote}.promisor")).is_some_and(|value| value == "true");
    let filter = get(&format!("remote.{remote}.partialclonefilter"));
    let views = git
        .run(&["worktree", "list", "--porcelain"])
        .map_or(0, |out| {
            out.lines()
                .filter(|line| line.starts_with("worktree ") && line.contains("/bay/views/"))
                .count()
        });
    let populated = !info.bare
        && git
            .run(&["ls-files", "-t"])
            .is_ok_and(|out| out.lines().any(|line| !line.starts_with('S')));
    let auto = get("maintenance.auto").is_none_or(|value| value != "false")
        && get("gc.auto").is_none_or(|value| value != "0");
    // Compared by real path: `git maintenance register` writes the path it was given, which on
    // macOS may run through a link (`/var` to `/private/var`).
    let real = |path: &str| std::fs::canonicalize(path).unwrap_or_else(|_| path.into());
    let here = real(&info.toplevel);
    let registered = git
        .run(&["config", "--get-all", "maintenance.repo"])
        .is_ok_and(|out| out.lines().any(|repo| real(repo) == here));
    let mut notes = Vec::new();
    if !info.bare && !promisor {
        notes.push("a full clone: the site may share a blob-less store (git clone --filter=blob:none --no-checkout)");
    }
    if promisor && populated {
        notes.push("a blob-less store with a populated work tree holds the blobs it checked out");
    }
    if !auto {
        notes.push(
            "auto maintenance is off: nothing repacks or prunes this store after a fetch or push",
        );
    }
    if !registered {
        notes.push("not registered for scheduled maintenance (git maintenance register)");
    }
    // G192: the commit-graph bay's path-limited walks read, from its files and config alone.
    let graph = crate::commit_graph::read(git);
    match graph.state {
        GraphState::Disabled => {
            notes.push("core.commitGraph is false: Git walks history without a commit-graph");
        }
        GraphState::Missing => notes.push(
            "no commit-graph: walks parse every commit (git commit-graph write --reachable --changed-paths)",
        ),
        GraphState::NoBloomFilters => notes.push(
            "no changed-path Bloom filters Git reads here: path-limited walks diff every tree \
             (git commit-graph write --reachable --changed-paths)",
        ),
        GraphState::Ok => {}
    }
    let fetch_writes = (!info.bare)
        .then(|| crate::commit_graph::config_bool(git, "fetch.writeCommitGraph") == Some(true));
    if fetch_writes == Some(false) {
        notes.push(
            "a fetch does not refresh the commit-graph (bay install sets fetch.writeCommitGraph)",
        );
    }
    json!({"name": "store",
        "state": if notes.is_empty() { "green" } else { "advisory" },
        "promisor_remote": promisor.then_some(remote),
        "filter": filter,
        "ref_format": info.ref_format,
        "views": views,
        "work_tree": if info.bare { "none" } else if populated { "populated" } else { "empty" },
        "maintenance_strategy": get("maintenance.strategy"),
        "auto_maintenance": auto,
        "scheduled": registered,
        "commit_graph": graph.detail(),
        "fetch_writes_commit_graph": fetch_writes,
        "notes": notes})
}

/// G174: the ref backend of a bay-hosted trunk. On `files` a multi-ref update is not one
/// atomic write, so a reader can see half of it; publication still works, through trunk as the
/// commit point, so the row is advisory. Only the host is judged: a forge's backend is its own.
fn ref_format_row(ref_format: &str) -> Value {
    if ref_format == "files" {
        json!({"name": "ref-format", "ref_format": ref_format, "state": "degraded",
            "note": "on files, readers can see half of a multi-ref update",
            "recovery": "git refs migrate --ref-format=reftable"})
    } else {
        json!({"name": "ref-format", "ref_format": ref_format, "state": "green"})
    }
}

/// The `claude` row: Claude Code against the hook minimum, where the adapter is configured.
fn claude_row(claude: &Claude, minimum: &Version, report: &mut Report) {
    let row = match claude {
        Claude::NotConfigured => {
            json!({"name": "claude", "state": "green", "note": "Claude adapter not configured"})
        }
        Claude::Absent => {
            report.red(Reason::ClaudeAbsent, None);
            json!({"name": "claude", "state": "red", "floor": CLAUDE_HOOK_MINIMUM,
                "note": "claude is absent", "recovery": CLAUDE_RECOVERY})
        }
        Claude::Found(version) if version < minimum => {
            report.red(Reason::ClaudeBelowFloor, None);
            json!({"name": "claude", "version": version.to_string(), "floor": CLAUDE_HOOK_MINIMUM,
                "state": "red", "recovery": CLAUDE_RECOVERY})
        }
        Claude::Found(version) => json!({"name": "claude", "version": version.to_string(),
            "floor": CLAUDE_HOOK_MINIMUM, "state": "green"}),
    };
    report.rows.push(row);
}

/// The config of the work repository at `dir`, and a `config` row when it has none or it
/// carries retired keys ([`ignored_row`]). With none,
/// the other rows name the bootstrap's remote and trunk where there is one, and otherwise the
/// defaults. The row is not red: the box is ready, and the repository's commands refuse
/// `CONFIG_MISSING` until trunk has a config.
fn repository_config(dir: &Path) -> Result<(Config, Option<Value>), String> {
    match Repo::open_with(dir, Fetch::Never) {
        Ok(repo) => {
            let row = ignored_row(&repo.config);
            Ok((repo.config, row))
        }
        Err(OpenError::ConfigMissing(_, detail)) => {
            let mut config = Config::default();
            if let Some(Ok(bootstrap)) = Bootstrap::of(dir) {
                config.trunk = Some(bootstrap.trunk);
                config.remote = Some(bootstrap.remote);
            }
            let row = json!({"name": "config", "required": false, "state": "missing",
                "reason": Reason::ConfigMissing.code(), "detail": detail});
            Ok((config, Some(row)))
        }
        Err(error) => Err(error.message()),
    }
}

/// A `config` row naming the retired keys the config carries, which bay accepts and never
/// reads (ruling Q14), when it carries any. Green: the config loads, and the policy is the one
/// source of every check command (G141).
fn ignored_row(config: &Config) -> Option<Value> {
    let ignored = config.ignored();
    (!ignored.is_empty()).then(|| {
        json!({"name": "config", "state": "green", "ignored": ignored,
            "note": "ignored: checks and their commands are declared in .bay/policy.toml, and \
                the update settings are the box's global Git config"})
    })
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
    // G158: the entry's timeout bounds a hung hook; without it the product's default holds the
    // agent first. Not red: the hook still fails closed when the default fires.
    if let Some(timeout) = agent_settings::claude_pre_tool_timeout(&settings) {
        row["pre_tool_timeout_s"] = json!(timeout);
        if timeout.is_none_or(|timeout| timeout > agent_settings::PRE_TOOL_TIMEOUT_S) {
            row["timeout_note"] = json!(format!(
                "bay's PreToolUse entry has no timeout of at most {} s, so a hung hook holds \
                 the agent for the product's default before it denies; run install again",
                agent_settings::PRE_TOOL_TIMEOUT_S
            ));
        }
    }
    // G169: an entry an earlier bay wrote has no presence test, so on a box of the fleet without
    // bay it denies every tool. Not red: this box has bay, and the entry still fails closed here.
    let outdated = agent_settings::outdated_claude_events(&settings);
    if !outdated.is_empty() {
        row["outdated"] = json!(outdated);
        row["outdated_note"] = json!(
            "these entries run bay without testing that it is installed, so on a box without bay \
             they deny every tool; run install again"
        );
    }
    if let Some(recorded) = recorded_unrelated_digest(git)? {
        row["unrelated_hooks_unchanged"] =
            json!(recorded == agent_settings::unrelated_digest(&settings));
    }
    report.rows.push(row);
    report
        .rows
        .push(claude_hooks_posture(&settings, missing.is_empty()));
    let home = crate::platform::home().map(std::path::PathBuf::from);
    hook_overrides_row(
        &agent_settings::claude_override_paths(Path::new(&info.toplevel), home.as_deref()),
        report,
    );
    Ok(())
}

/// The `hook-overrides` row (G154, R22): the settings files the agent product reads beside the
/// project's that switch bay's hooks off, by `disableAllHooks: true` or
/// `allowManagedHooksOnly: true`. Red with `HOOKS_MISSING`, naming each file; a file that cannot
/// be read is named too, since what it does is unknown. Another tool's hooks for bay's events
/// are listed under `added`: they run beside bay's. Doctor only reads them.
fn hook_overrides_row(paths: &[std::path::PathBuf], report: &mut Report) {
    let mut overrides = Vec::new();
    let mut added = Vec::new();
    for path in paths {
        let file = path.display().to_string();
        let settings = match agent_settings::read(path) {
            Ok(settings) => settings,
            Err(why) => {
                overrides.push(json!({"file": file, "unreadable": why}));
                continue;
            }
        };
        let disabled = settings.get("disableAllHooks") == Some(&Value::Bool(true));
        let managed_only = settings.get("allowManagedHooksOnly") == Some(&Value::Bool(true));
        // Claude Code runs the hooks of every settings scope together, so another tool's entry
        // for one of bay's events adds to bay's, never replaces it (R22). It is listed, and
        // only a setting that switches hooks off weakens bay's.
        let events = agent_settings::claude_events_without_bay(&settings);
        if !events.is_empty() {
            added.push(json!({"file": file, "events": events}));
        }
        if disabled || managed_only {
            overrides.push(json!({"file": file, "disable_all_hooks": disabled,
                "allow_managed_hooks_only": managed_only}));
        }
    }
    if overrides.is_empty() {
        report
            .rows
            .push(json!({"name": "hook-overrides", "state": "green",
            "read": paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
            "overrides": [], "added": added}));
        return;
    }
    let files: Vec<&str> = overrides
        .iter()
        .filter_map(|found| found["file"].as_str())
        .collect();
    report.red(
        Reason::HooksMissing,
        Some("local settings weaken bay's hooks"),
    );
    report.rows.push(json!({
        "name": "hook-overrides",
        "state": "red",
        "overrides": overrides,
        "added": added,
        "recovery": format!(
            "edit {}: remove disableAllHooks and allowManagedHooksOnly, which switch bay's hooks off",
            files.join(", ")
        ),
    }));
}

/// The `agent-hooks` row of one agent product (H4): whether its early refusal fails closed,
/// read from the `onFailure` of bay's installed `PreToolUse` entry, and that `Stop` cannot
/// (Claude Code ignores `onFailure` there, so the Stop refusal is a once-only nudge, ruling R7),
/// and that the deny holds under `bypassPermissions` (G153, the G61 probes). Informational: red only when bay's entries are missing, and the `hooks` row already gives
/// that reason. The guarantee never rests on this hook: land and the host hook re-judge.
fn claude_hooks_posture(settings: &serde_json::Map<String, Value>, entries_present: bool) -> Value {
    let on_failure = agent_settings::claude_pre_tool_on_failure(settings);
    let early_refusal = match &on_failure {
        None => "none",
        Some(Some(value)) if value == "block" => "fail_closed",
        Some(_) => "fail_open",
    };
    json!({
        "name": "agent-hooks",
        "product": "claude",
        "state": state(entries_present),
        "early_refusal": early_refusal,
        "pre_tool": {"event": "PreToolUse", "on_failure": on_failure.flatten()},
        "stop": {"event": "Stop", "fails_closed": false,
            "why": "the product ignores onFailure on Stop; the Stop refusal is asked once"},
        // G153: the G61 probes P5a and P5b ran the installed entry under both bypass flags.
        "bypass_permissions": {"deny_holds": entries_present,
            "why": "PreToolUse hooks run in bypassPermissions and exit 2 still denies (probed on Claude Code 2.1.303); session rows name each session's permission_mode"},
        "guarantee": "land and trunk hook (G8, G23)",
        "note": "the guarantee does not rest on this hook: land and the host hook re-judge every change",
    })
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

/// The `repository` row (G157): HEAD's distance from the trunk tracking ref, ahead and behind,
/// from local refs only (doctor never fetches, so behind is as of the last fetch), and the
/// count of tracked files that differ from HEAD, so a checkout that runs code no commit
/// describes is named. Informational: green, with a note when HEAD is behind or files are
/// modified. `git status` runs without optional locks, so the read writes no index.
fn repository_row(git: &Git, config: &Config, report: &mut Report) {
    let trunk = format!("refs/remotes/{}/{}", config.remote(), config.trunk());
    let distance = git
        .run(&[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...{trunk}"),
        ])
        .ok()
        .and_then(|counts| {
            let (ahead, behind) = counts.split_once(char::is_whitespace)?;
            Some((
                ahead.trim().parse::<u64>().ok()?,
                behind.trim().parse::<u64>().ok()?,
            ))
        });
    let modified = git
        .run(&[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=no",
        ])
        .ok()
        .map(|status| status.lines().count());
    let mut notes = Vec::new();
    match distance {
        None => notes.push(format!(
            "no distance from {trunk}: HEAD or the trunk tracking ref is missing"
        )),
        Some((_, behind)) if behind > 0 => notes.push(format!(
            "HEAD is {behind} commits behind {trunk} as of the last fetch"
        )),
        Some(_) => {}
    }
    match modified {
        None => notes.push("the modified tracked files could not be read".to_owned()),
        Some(count) if count > 0 => notes.push(format!(
            "{count} tracked files differ from HEAD: this checkout runs code no commit describes"
        )),
        Some(_) => {}
    }
    let mut row = json!({
        "name": "repository",
        "required": false,
        "state": "green",
        "trunk": trunk,
        "ahead": distance.map(|(ahead, _)| ahead),
        "behind": distance.map(|(_, behind)| behind),
        "modified": modified,
        "fetched": false,
    });
    if !notes.is_empty() {
        row["note"] = json!(notes.join("; "));
    }
    report.rows.push(row);
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

/// The `key-custody` row (G145, `crate::custody`), on every box. In a work repository its
/// session records are this machine's evidence of which account runs agent sessions.
fn custody_row(git: &Git, info: Option<&RepoInfo>, report: &mut Report) {
    let sessions = info.filter(|info| !info.bare).and_then(|_| {
        git.run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .ok()
            .map(|common| Path::new(&common).join("bay").join("sessions"))
    });
    let custody = crate::custody::row(git, sessions.as_deref());
    if custody.exposed {
        report.red(
            Reason::KeyExposed,
            Some("a signing key is readable by the session account"),
        );
    }
    report.rows.push(custody.row);
}

/// The `environments` row (`INTERFACES` § 8): the policy environments this box's executor
/// serves (`bay.checkEnvironment`, § 7), and whether the box declared them or takes the
/// default, every environment, as before the declaration existed. Under the default, `serves`
/// lists the environments of the policy at trunk as this box last read it. For a stated
/// executor (G63) a declaration that names none is red with `CHECK_UNAVAILABLE`: it would run no
/// check. An agent view only reports it.
fn environments_row(git: &Git, config: &Config, executor: bool, report: &mut Report) {
    let served = crate::check::Served::read(git);
    let trunk = format!("refs/remotes/{}/{}", config.remote(), config.trunk());
    let mut needed: Vec<String> = crate::policy::load_at(git, &trunk)
        .ok()
        .flatten()
        .map(|policy| {
            policy
                .checks
                .iter()
                .filter(|entry| !policy.is_quarantined(entry))
                .map(|entry| entry.environment.clone())
                .collect()
        })
        .unwrap_or_default();
    needed.sort();
    needed.dedup();
    let listed: Vec<String> = match &served {
        crate::check::Served::Every => needed.clone(),
        crate::check::Served::Declared(declared) => declared.clone(),
    };
    let none = matches!(&served, crate::check::Served::Declared(declared) if declared.is_empty());
    let mut row = json!({"name": "environments", "declared": served.declared(),
        "source": served.source(), "serves": listed, "required": executor,
        "state": if !executor { "informational" } else if none { "red" } else { "green" }});
    if served == crate::check::Served::Every {
        row["note"] = json!(format!(
            "{} is not set: this box serves every environment; declare one value per environment to serve only those",
            crate::check::ENVIRONMENT_KEY
        ));
    }
    let unserved: Vec<&String> = needed
        .iter()
        .filter(|environment| !served.serves(environment))
        .collect();
    if !unserved.is_empty() {
        row["unserved"] = json!(unserved);
    }
    if none {
        row["recovery"] = json!(format!(
            "git config --global --add {} <environment>",
            crate::check::ENVIRONMENT_KEY
        ));
        if executor {
            report.red(
                Reason::CheckUnavailable,
                Some("this box serves no check environment"),
            );
        }
    }
    report.rows.push(row);
}

/// For a stated executor (G63): each `env` name of a required check in the policy at trunk, as
/// this box last read it, is set in this environment. An executor that lacks one records that
/// check as `infra_failure` (ruling 8b), so the box is red with `CHECK_ENV_UNSET` before the
/// first landing instead. A missing or invalid policy is the landing's refusal, not this row's.
fn check_env_row(git: &Git, config: &Config, report: &mut Report) {
    let trunk = format!("refs/remotes/{}/{}", config.remote(), config.trunk());
    let policy = match crate::policy::load_at(git, &trunk) {
        Ok(Some(policy)) => policy,
        Ok(None) => {
            report.rows.push(
                json!({"name": "check-env", "required": true, "state": "green",
                "note": "no policy at trunk"}),
            );
            return;
        }
        Err(error) => {
            report
                .rows
                .push(json!({"name": "check-env", "required": true,
                "state": "not judged", "note": String::from(error)}));
            return;
        }
    };
    let unset: Vec<Value> = policy
        .checks
        .iter()
        .filter(|entry| !policy.is_quarantined(entry))
        .flat_map(|entry| {
            entry
                .env
                .iter()
                .filter(|name| std::env::var_os(name).is_none())
                .map(|name| {
                    json!({"check": entry.id, "environment": entry.environment, "env": name})
                })
        })
        .collect();
    if unset.is_empty() {
        report
            .rows
            .push(json!({"name": "check-env", "required": true, "state": "green"}));
        return;
    }
    report.red(Reason::CheckEnvUnset, Some("a check's env name is unset"));
    report.rows.push(
        json!({"name": "check-env", "required": true, "state": "red",
        "unset": unset, "recovery": "set each named variable in the executor's environment"}),
    );
}

/// For a stated executor under a registry (G63, § 10a): the executor key that signs its
/// publications and the producer key that signs its evidence are set, and each one's name is
/// enrolled in its class file. Without a registry the executor publishes unsigned: green.
fn executor_keys_row(git: &Git, keys: &crate::signing::Keys, report: &mut Report) {
    use crate::registry::{KeyClass, Registry};
    let registry = match Registry::current(git) {
        Ok(None) => {
            report
                .rows
                .push(json!({"name": "executor-keys", "required": false,
                "state": "green", "note": "no registry: the executor publishes unsigned"}));
            return;
        }
        Ok(Some(registry)) => registry,
        Err(why) => {
            report.red(Reason::ExecutorKeyMissing, Some("registry unreadable"));
            report
                .rows
                .push(json!({"name": "executor-keys", "required": true,
                "state": "red", "note": why}));
            return;
        }
    };
    let judge = |set: bool, class: KeyClass, name: &str| match (set, registry.enrols(class, name)) {
        (false, _) => "not set",
        (true, true) => "enrolled",
        (true, false) => "not enrolled",
    };
    let executor = judge(
        keys.executor.is_some(),
        KeyClass::Executor,
        &keys.executor_name,
    );
    let producer = judge(
        keys.producer.is_some(),
        KeyClass::Producer,
        &keys.producer_name,
    );
    let green = executor == "enrolled" && producer == "enrolled";
    let mut row = json!({"name": "executor-keys", "required": true, "state": state(green),
        "executor": executor, "executor_name": keys.executor_name,
        "producer": producer, "producer_name": keys.producer_name});
    if !green {
        report.red(
            Reason::ExecutorKeyMissing,
            Some("executor keys not enrolled"),
        );
        row["recovery"] = json!(
            "set bay.executorKey and bay.producerKey, under the names bay.executorName and bay.producerName that the registry enrols"
        );
    }
    report.rows.push(row);
}

/// The check sandbox (ruling R14): red with `SANDBOX_ABSENT` on a box in the executor or
/// producer role (it has `bay.executorKey` or `bay.producerKey`) where no check would run for
/// want of a sandbox. A box in the agent-view role runs no checks, so it is not red there.
fn sandbox_row(sandbox: &crate::check::Sandbox, runs_checks: bool, report: &mut Report) {
    let row = match (&sandbox.refusal, runs_checks) {
        (None, _) if sandbox.kind == "none" => json!({"name": "sandbox", "kind": "none",
            "required": runs_checks, "state": "green",
            "note": "no check sandbox; this box chose to run checks without one (bay.executorSandbox = none)"}),
        (None, _) => json!({"name": "sandbox", "kind": sandbox.kind, "required": runs_checks,
            "state": "green"}),
        (Some(_), false) => json!({"name": "sandbox", "kind": sandbox.kind, "required": false,
            "state": "green", "note": "no check sandbox; this box runs no checks (agent-view role)"}),
        (Some(why), true) => {
            report.red(Reason::SandboxAbsent, Some("no check sandbox"));
            let recovery = crate::check::apparmor_remedy_here().unwrap_or_else(|| {
                "install the check sandbox, or choose to run checks without one: git config --global bay.executorSandbox none".to_owned()
            });
            json!({"name": "sandbox", "kind": sandbox.kind, "required": true, "state": "red",
                "note": why, "recovery": recovery})
        }
    };
    report.rows.push(row);
}

/// The installed skill at `$HOME/.claude/skills/bay/` (or `$CLAUDE_CONFIG_DIR/skills/bay/`
/// when that is set, the path install writes, G65) against the skill this bay version
/// installs (§ 15.2, G49). A consumer repository's skills are never this copy. A mismatch is
/// red on every box; a missing copy is red only where the Claude adapter is configured.
fn skill_row(config: &Config, report: &mut Report) -> Result<(), String> {
    let home =
        crate::platform::home().ok_or("HOME is not set; doctor cannot name the skill path")?;
    let path = skill::installed_path(Path::new(&home), skill::config_dir().as_deref());
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
    let mut cmd = Command::new(crate::platform::program("claude"));
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

#[cfg(test)]
mod tests {
    use super::{Report, sandbox_row};
    use crate::check::Sandbox;
    use crate::result::Reason;

    fn row(sandbox: &Sandbox, runs_checks: bool) -> (serde_json::Value, Option<Reason>) {
        let mut report = Report::default();
        sandbox_row(sandbox, runs_checks, &mut report);
        (report.rows.remove(0), report.first_red)
    }

    /// Ruling R14: with no sandbox and no choice by the box, the row is red only on a box in
    /// the executor or producer role; an agent-view box runs no checks.
    #[test]
    fn the_sandbox_row_is_red_only_for_a_box_that_runs_checks() {
        let none = Sandbox::judge("none", Ok(false));
        let (red, reason) = row(&none, true);
        assert_eq!(red["state"], "red");
        assert!(matches!(reason, Some(Reason::SandboxAbsent)));
        let (agent_view, reason) = row(&none, false);
        assert_eq!(agent_view["state"], "green");
        assert!(reason.is_none());
        let (chose_none, reason) = row(&Sandbox::judge("none", Ok(true)), true);
        assert_eq!(chose_none["state"], "green");
        assert_eq!(chose_none["kind"], "none");
        assert!(reason.is_none());
    }
}

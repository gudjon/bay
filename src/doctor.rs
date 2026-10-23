//! `bay doctor`: the capability probe (`DESIGN` § 15.2, `INTERFACES` § 8), and the `bay`
//! row of `bay update` (installed version, newest recorded release, pin).
//!
//! Doctor reads. It writes no settings file and no Git config. In a work repository whose
//! remote answers, it publishes its verdict on this machine's live summary (G314,
//! `crate::live::publish_doctor`), so `bay status --fleet` shows every machine's readiness.

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
    CLAUDE_HOOK_MINIMUM, CODEX_HOOK_MINIMUM, GIT_FLOOR_AGENT_VIEW, GIT_FLOOR_ANY,
    GIT_FLOOR_EXECUTOR, GIT_FLOOR_HOSTED_TRUNK, GIT_RECOMMENDED, GIT_SHARED_CACHE, Version,
};

/// Where `bay install` records the digest of the hook entries that are not bay's.
pub const UNRELATED_HOOKS_KEY: &str = "bay.unrelated-hooks-digest";

const CLAUDE_RECOVERY: &str = "upgrade Claude Code or run without the Claude adapter";

const CODEX_RECOVERY: &str = "upgrade Codex or run without the Codex adapter";

/// An agent product's program on this box, where its adapter is configured.
enum Program {
    NotConfigured,
    Absent,
    Found(Version),
}

#[derive(Default)]
struct Report {
    rows: Vec<Value>,
    first_red: Option<Reason>,
    /// The tool the first red reason names, for `TOOL_BELOW_FLOOR` and `TOOL_ABSENT`.
    first_tool: Option<&'static str>,
    /// The first red row that is not a version, as the doctor field names it.
    doctor_note: Option<&'static str>,
    /// G305: whether bay guards this repository's Git here (`detail.guard`).
    guard: Option<Value>,
}

impl Report {
    fn red(&mut self, reason: Reason, note: Option<&'static str>) {
        self.first_red.get_or_insert(reason);
        if let Some(note) = note {
            self.doctor_note.get_or_insert(note);
        }
    }

    /// [`Report::red`] for a tool below its floor or absent: `detail.tool` names it when this is
    /// the first red reason.
    fn red_tool(&mut self, reason: Reason, tool: &'static str, note: Option<&'static str>) {
        if self.first_red.is_none() {
            self.first_tool = Some(tool);
        }
        self.red(reason, note);
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
        Ok(result) => {
            publish(dir, &result);
            result
        }
        Err(message) => BayResult::failed(message),
    }
}

/// G314: this machine's verdict as its live summary carries it, derived from doctor's result
/// alone: when, which bay, the decision and reason, the names of the red rows, whether bay's
/// hooks are `on`, `off` or `missing`, the state of the `sandbox` row, and the role. No path
/// and no secret.
fn verdict(result: &BayResult) -> Value {
    let detail = result.detail.as_ref().unwrap_or(&Value::Null);
    let rows: &[Value] = detail["rows"].as_array().map_or(&[][..], Vec::as_slice);
    let named = |name: &str| rows.iter().find(|row| row["name"] == name);
    let red: Vec<&Value> = rows
        .iter()
        .filter(|row| row["state"] == "red")
        .map(|row| &row["name"])
        .collect();
    let hooks = if detail["guard"]["hook"] == "off" {
        "off"
    } else if named("hooks").is_some_and(|row| row["state"] == "green") {
        "on"
    } else {
        "missing"
    };
    json!({
        "at": crate::clock::now_rfc3339(),
        "bay_version": crate::version::BAY,
        "decision": result.decision.word(),
        "reason": result.reason.map(Reason::code),
        "red": red,
        "hooks": hooks,
        "sandbox": named("sandbox").map(|row| row["state"].clone()),
        "role": detail["role"],
    })
}

/// G314: publishes [`verdict`] on this machine's live summary, when doctor ran in a work
/// repository whose remote answered. Best effort ([`crate::live::publish_doctor`]): nothing
/// here changes `result`.
fn publish(dir: &Path, result: &BayResult) {
    let Some(detail) = &result.detail else {
        return;
    };
    let answered = detail["rows"].as_array().is_some_and(|rows| {
        rows.iter()
            .any(|row| row["name"] == "remote" && row["state"] == "green")
    });
    if !answered {
        return;
    }
    let Ok(repo) = Repo::open(dir) else {
        return;
    };
    crate::live::publish_doctor(&repo, verdict(result));
}

/// The role of this box, which names its Git floor (`DESIGN` § 15.2, `GIT-FEATURES` § 1).
#[derive(Clone, Copy)]
enum Role {
    /// A repository that is not a bay-hosted trunk.
    AgentView,
    /// The bare repository `bay host init` creates (§ 10a).
    HostedTrunk,
    /// A box that runs `bay runner run`, as `--role executor` states (G63).
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

/// The repository doctor runs in; `None` outside one, where the machine rows still answer and
/// the repository rows do not apply.
fn repo_here(git: &Git) -> Result<Option<RepoInfo>, String> {
    if git.outside_repository().map_err(|error| error.message())? {
        Ok(None)
    } else {
        git.repo_info().map(Some).map_err(|error| error.message())
    }
}

fn probe(dir: &Path, executor: bool) -> Result<BayResult, String> {
    let git = Git::new(dir);
    let git_line = git.version().map_err(|error| error.message())?;
    let git_version = Version::parse(&git_line);
    let info = repo_here(&git)?;
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

    let minimum = Version::parse(CLAUDE_HOOK_MINIMUM);
    let claude = program_rows(&config, &minimum, &mut report)?;

    if let Some(info) = &info {
        report.rows.extend(config_row);
        remote_row(&git, &config, info.bare, &mut report)?;
        hooks_row(&git, &config, info, &mut report)?;
        if info.bare {
            report.rows.push(ref_format_row(&info.ref_format));
        }
        report.rows.push(store_row(&git, &config, info));
        gc_log_row(&git, &mut report);
        quarantine_row(&git, &config, info.bare, &mut report);
        signing_row(&git, &mut report);
        if !info.bare {
            repository_row(&git, &config, &mut report);
            live_row(&git, &mut report);
            environments_row(&git, &config, executor, &mut report);
        }
        if executor && !info.bare {
            check_env_row(&git, &config, &mut report);
            executor_keys_row(&git, &keys, &mut report);
            presence_row(&git, &config, &keys, &mut report);
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

    skill_row(&git, &config, &mut report)?;
    custody_row(&git, &config, info.as_ref(), &mut report);
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
    read_paths_row(&git, &mut report);

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
    let mut detail = json!({
        "bay": crate::version::BAY,
        "git": git_line,
        "repository": info.as_ref().map(|info| &info.toplevel),
        "object format": info.as_ref().map(|info| &info.object_format),
        "ref format": info.as_ref().map(|info| &info.ref_format),
        "branch": info.as_ref().and_then(|info| info.branch.as_ref()),
        "role": role,
        "role_source": role_source,
        "rows": report.rows,
        "guard": report.guard,
    });
    if let Some(tool) = report.first_tool {
        detail["tool"] = json!(tool);
    }
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

/// G265 (N-13): a `gc.log` in the repository's common dir, present only when it is there. Git
/// leaves it when a background `git gc --auto` failed, and refuses every further auto gc while
/// it is newer than `gc.logExpiry`, so loose objects pile up on the box's disk unseen. Advisory:
/// no reason code names it.
fn gc_log_row(git: &Git, report: &mut Report) {
    let Ok(common) = git.run(&["rev-parse", "--path-format=absolute", "--git-common-dir"]) else {
        return;
    };
    let file = Path::new(common.trim()).join("gc.log");
    if !file.is_file() {
        return;
    }
    report.rows.push(json!({
        "name": "gc-log",
        "state": "degraded",
        "file": file.display().to_string(),
        "note": "a background git gc --auto failed, and git runs no auto gc while this file stays",
        "recovery": "read the file, fix what it names, then remove it",
    }));
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
fn claude_row(claude: &Program, minimum: &Version, report: &mut Report) {
    let row = match claude {
        Program::NotConfigured => {
            json!({"name": "claude", "state": "green", "note": "Claude adapter not configured"})
        }
        Program::Absent => {
            report.red_tool(Reason::ToolAbsent, "claude", None);
            json!({"name": "claude", "state": "red", "floor": CLAUDE_HOOK_MINIMUM,
                "note": "claude is absent", "recovery": CLAUDE_RECOVERY})
        }
        Program::Found(version) if version < minimum => {
            report.red_tool(Reason::ToolBelowFloor, "claude", None);
            json!({"name": "claude", "version": version.to_string(), "floor": CLAUDE_HOOK_MINIMUM,
                "state": "red", "recovery": CLAUDE_RECOVERY})
        }
        Program::Found(version) => json!({"name": "claude", "version": version.to_string(),
            "floor": CLAUDE_HOOK_MINIMUM, "state": "green"}),
    };
    report.rows.push(row);
}

/// The rows of the agent products' programs: `claude` on every box, and `codex` where the Codex
/// adapter is configured (G308). Claude Code's program, which the status line names.
fn program_rows(
    config: &Config,
    minimum: &Version,
    report: &mut Report,
) -> Result<Program, String> {
    let claude = if config.claude_adapter() {
        probe_claude()?
    } else {
        Program::NotConfigured
    };
    claude_row(&claude, minimum, report);
    if config.codex_adapter() {
        codex_row(&probe_program("codex")?, report);
    }
    Ok(claude)
}

/// The `codex` row (G308): Codex against the version its payloads were recorded on, where the
/// Codex adapter is configured, as the `claude` row judges Claude Code.
fn codex_row(codex: &Program, report: &mut Report) {
    let minimum = Version::parse(CODEX_HOOK_MINIMUM);
    let row = match codex {
        Program::NotConfigured => return,
        Program::Absent => {
            report.red_tool(Reason::ToolAbsent, "codex", Some("codex is absent"));
            json!({"name": "codex", "state": "red", "floor": CODEX_HOOK_MINIMUM,
                "note": "codex is absent", "recovery": CODEX_RECOVERY})
        }
        Program::Found(version) if *version < minimum => {
            report.red_tool(
                Reason::ToolBelowFloor,
                "codex",
                Some("codex below its floor"),
            );
            json!({"name": "codex", "version": version.to_string(), "floor": CODEX_HOOK_MINIMUM,
                "state": "red", "recovery": CODEX_RECOVERY})
        }
        Program::Found(version) => json!({"name": "codex", "version": version.to_string(),
            "floor": CODEX_HOOK_MINIMUM, "state": "green"}),
    };
    report.rows.push(row);
}

/// The config of the work repository at `dir`, and a `config` row when it has none or carries
/// retired keys ([`loaded_row`]). With none, the other rows name the bootstrap's remote and
/// main where there is one, and otherwise the defaults. A missing config's row is not red: the
/// machine is ready, and the repository's commands refuse `CONFIG_MISSING` until main has a
/// config.
fn repository_config(dir: &Path) -> Result<(Config, Option<Value>), String> {
    let bootstrapped = || {
        let mut config = Config::default();
        if let Some(Ok(bootstrap)) = Bootstrap::of(dir) {
            config.trunk = Some(bootstrap.trunk);
            config.remote = Some(bootstrap.remote);
        }
        config
    };
    match Repo::open_with(dir, Fetch::Never) {
        Ok(repo) => {
            let row = loaded_row(&repo.config);
            Ok((repo.config, row))
        }
        Err(OpenError::ConfigMissing(_, detail)) => {
            let row = json!({"name": "config", "required": false, "state": "missing",
                "reason": Reason::ConfigMissing.code(), "detail": detail});
            Ok((bootstrapped(), Some(row)))
        }
        Err(error) => Err(error.message()),
    }
}

/// A green `config` row, when the config carries retired keys, which bay accepts and never
/// reads (ruling Q14). Green: the config loads, and the policy is the one source of every check
/// command (G141).
fn loaded_row(config: &Config) -> Option<Value> {
    let ignored = config.ignored();
    if ignored.is_empty() {
        return None;
    }
    let note = "ignored: checks and their commands are declared in .bay/policy.toml, and the \
                update settings are this machine's global Git config";
    Some(json!({"name": "config", "state": "green", "ignored": ignored, "note": note}))
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
        report.red_tool(Reason::ToolBelowFloor, "git", None);
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

/// G305 (owner ruling 2026-10-21, red only on evidence): with no agent adapter declared, bay's
/// hook never judges an agent's Git command here, so an agent can push the trunk around bay.
/// That is a choice, and `off` alone is advisory; trunk landings that went around bay, on a
/// trunk bay also lands on, are the evidence that it costs something, and turn it red. The landings are the trunk's newest
/// window as the last fetch left it (`crate::unattributed`), the count `bay status` names.
fn unguarded(git: &Git, config: &Config, report: &mut Report) {
    let mut row = json!({
        "name": "hooks",
        "state": "off",
        "note": "no agent adapter is declared, so bay does not judge an agent's Git commands here: an agent can push main around bay",
        "recovery": "add [agent] product = \"claude\" (or \"codex\") to .bay/config.toml on main, run bay install in one clone, and merge the settings file it writes",
    });
    let mut guard = json!({"hook": "off"});
    if let Some((count, through)) = around_bay(git, config)
        && count > 0
        && through > 0
    {
        let around = json!({"count": count, "of": count + through});
        row["state"] = json!("red");
        row["around_bay"] = around.clone();
        guard["around_bay"] = around;
        report.red(
            Reason::HooksMissing,
            Some("bay does not guard this repository, and merges into main went around it"),
        );
    }
    report.guard = Some(guard);
    report.rows.push(row);
}

/// G305: of the trunk's newest window of landings, as the last fetch left the trunk, how many
/// went around bay and how many came through it: `(around, through)`. Only a trunk bay lands
/// on can be gone around: one with no bay landing in the window has not taken bay up yet.
/// `None` when the trunk cannot be read here.
fn around_bay(git: &Git, config: &Config) -> Option<(u64, u64)> {
    let tracking = format!("refs/remotes/{}/{}", config.remote(), config.trunk());
    let trunk = git.run(&["rev-parse", "--verify", "-q", &tracking]).ok()?;
    let scan = crate::unattributed::scan(git, trunk.trim()).ok()?;
    Some((scan["count"].as_u64()?, scan["through_bay"].as_u64()?))
}

/// G320 (learned in a fleet rollout): the step that puts bay's hook entries in place. When the
/// repository tracks the agent settings `file`, the entries reach every clone only with the
/// change that commits them, so the step is to land that change: `bay install` here would
/// only leave an uncommitted edit. Otherwise `bay install` writes them in this clone.
fn hooks_recovery(git: &Git, file: &str) -> String {
    let tracked = git
        .run(&["ls-files", "--error-unmatch", "--", file])
        .is_ok();
    if tracked {
        format!(
            "{file} is tracked: run bay install in one clone, then merge the change that commits its entries; each clone gets them when it takes that change"
        )
    } else {
        "run install again".to_owned()
    }
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
    if config.codex_adapter() {
        return codex_hooks_rows(git, info, report);
    }
    if !config.claude_adapter() {
        unguarded(git, config, report);
        return Ok(());
    }
    report.guard = Some(json!({"hook": "on"}));
    let path = agent_settings::claude_settings_path(Path::new(&info.toplevel));
    let settings = agent_settings::read(&path)?;
    let missing = agent_settings::missing_claude_events(&settings);
    let mut row = json!({"name": "hooks", "state": state(missing.is_empty())});
    if !missing.is_empty() {
        row["missing"] = json!(missing);
        row["recovery"] = json!(hooks_recovery(git, agent_settings::CLAUDE_SETTINGS_FILE));
        report.red(Reason::HooksMissing, Some("hooks missing"));
    }
    // G158: the entry's timeout bounds a hung hook; without it the product's default holds the
    // agent first. Not red: the hook still fails closed when the default fires.
    if let Some(timeout) = agent_settings::pre_tool_timeout(&settings) {
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
    // G324: an entry an older bay wrote under a name 0.3.0 no longer knows (H1, no aliases).
    let renamed = agent_settings::renamed_claude_events(&settings);
    if !renamed.is_empty() {
        row["old_names"] = json!(renamed);
        row["old_names_note"] = json!(
            "these entries call bay hook under a name an older bay wrote, which this bay \
             refuses; run install again"
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

/// The `hooks` and `agent-hooks` rows of the Codex adapter (G308): bay's entries in
/// `.codex/hooks.json`, red with `HOOKS_MISSING` when one is missing, and the posture. Codex
/// runs a tool whose hook crashes, hangs or exits other than 2 (probed on 0.153.2) and has no
/// setting that changes it, so bay's deny there is advisory: the `agent-hooks` row, where
/// doctor states every product's early refusal, says `fail_open`, and land and the trunk hook
/// stay the guard.
fn codex_hooks_rows(git: &Git, info: &RepoInfo, report: &mut Report) -> Result<(), String> {
    let path = agent_settings::codex_hooks_path(Path::new(&info.toplevel));
    let settings = agent_settings::read(&path)?;
    let missing = agent_settings::missing_codex_events(&settings);
    let present = missing.is_empty();
    let mut row = json!({
        "name": "hooks",
        "product": "codex",
        "file": agent_settings::CODEX_HOOKS_FILE,
        "state": state(present),
        "trust_note": "Codex runs these entries only in a trusted project, once its hooks are trusted",
    });
    if !present {
        row["missing"] = json!(missing);
        row["recovery"] = json!(hooks_recovery(git, agent_settings::CODEX_HOOKS_FILE));
        report.red(Reason::HooksMissing, Some("hooks missing"));
    }
    if let Some(timeout) = agent_settings::pre_tool_timeout(&settings) {
        row["pre_tool_timeout_s"] = json!(timeout);
    }
    if let Some(recorded) = recorded_unrelated_digest(git)? {
        row["unrelated_hooks_unchanged"] =
            json!(recorded == agent_settings::unrelated_digest(&settings));
    }
    report.rows.push(row);
    report.rows.push(json!({
        "name": "agent-hooks",
        "product": "codex",
        "state": state(present),
        "early_refusal": if present { "fail_open" } else { "none" },
        "early_refusal_note": "Codex runs the tool when bay's hook crashes, hangs or fails, so bay's deny there is advisory; merge and the main hook are the guard",
        "pre_tool": {"event": "PreToolUse", "fails_closed": false,
            "why": "exit 2 denies; a crash, a timeout or another exit lets the tool run (probed on Codex 0.153.2)"},
        "stop": {"event": "Stop", "fails_closed": false,
            "why": "no recording shows Codex keeping a session going for a stop hook"},
        "guarantee": "merge and main hook (G8, G23)",
        "note": "the guarantee does not rest on this hook: merge and the host hook re-judge every change",
    }));
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
        "guarantee": "merge and main hook (G8, G23)",
        "note": "the guarantee does not rest on this hook: merge and the host hook re-judge every change",
    })
}

/// The open quarantines of the policy at trunk as this box last read it (`DESIGN` § 7: an
/// overdue quarantine is shown by `bay doctor`). Present only when a quarantine is open, or as
/// `not judged` when the policy cannot be read (G104b). It is never red: `INTERFACES` § 8 has
/// no reason code for it, and an overdue quarantine stays in force until the fixing change
/// removes it.
fn quarantine_row(git: &Git, config: &Config, bare: bool, report: &mut Report) {
    let trunk = if bare {
        let name = git
            .run(&["config", "--get", host::TRUNK_KEY])
            .unwrap_or_else(|_| config.trunk().to_owned());
        format!("refs/heads/{name}")
    } else {
        format!("refs/remotes/{}/{}", config.remote(), config.trunk())
    };
    let open = match crate::status::open_quarantines(git, &trunk) {
        Ok(open) => open,
        // G104b: a policy this box cannot read is no judgement of its quarantines.
        Err(why) => {
            report
                .rows
                .push(json!({"name": "quarantine", "required": false,
                "state": "not judged", "note": why}));
            return;
        }
    };
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
/// session records are this machine's evidence of which account runs agent sessions, and the
/// row names whether the policy at trunk requires actor proof (G243b): `true`, `false`, or the
/// reason it could not be read.
fn custody_row(git: &Git, config: &Config, info: Option<&RepoInfo>, report: &mut Report) {
    let sessions = info.filter(|info| !info.bare).and_then(|_| {
        git.run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .ok()
            .map(|common| Path::new(&common).join("bay").join("sessions"))
    });
    let mut custody = crate::custody::row(git, sessions.as_deref());
    if let Some(info) = info {
        let trunk = if info.bare {
            let name = git
                .run(&["config", "--get", host::TRUNK_KEY])
                .unwrap_or_else(|_| config.trunk().to_owned());
            format!("refs/heads/{name}")
        } else {
            format!("refs/remotes/{}/{}", config.remote(), config.trunk())
        };
        let required = match crate::policy::requires_actor_proof_at(git, &trunk) {
            Ok(required) => json!(required),
            Err(why) => json!({"unread": why}),
        };
        // The judgement land makes (`crate::custody::actor_proof`), so the two never disagree.
        let refused = required == json!(true)
            && crate::custody::actor_proof(git, sessions.as_deref()).unproven();
        custody.row["require_actor_proof"] = required;
        if refused {
            custody.row["actor_refusal"] = json!(
                "the policy on main sets [agent] require_actor_proof: merge is refused ACTOR_UNPROVEN on this machine"
            );
        }
    }
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
                .filter(|entry| !policy.is_quarantined(entry) && !entry.is_approval())
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

/// The `signing` row (N-21), never red: whether this repository has a key registry, and what it
/// signs under the policy at trunk, `all` or `approvals` (`[policy] signing`). Without a
/// registry nothing is signed, and no approval can count.
fn signing_row(git: &Git, report: &mut Report) {
    let row = match crate::registry::Registry::current(git) {
        Ok(None) => json!({"name": "signing", "required": false, "state": "green",
            "registry": null, "mode": "unsigned",
            "note": "no key registry: nothing is signed, and no approval can count"}),
        Ok(Some(registry)) if registry.signs_all() => json!({"name": "signing",
            "required": false, "state": "green", "registry": registry.commit,
            "mode": registry.mode().name(),
            "note": "merges, requests, chains and evidence are signed by enrolled keys"}),
        Ok(Some(registry)) => json!({"name": "signing", "required": false, "state": "green",
            "registry": registry.commit, "mode": registry.mode().name(),
            "note": "only approvals are signed, by keys of the registry's approver file; landings, requests, chains and other evidence run unsigned"}),
        // 0.3.3: a registry no version rule allows breaks every signed judgement: red, named.
        Err(why) => {
            report.red(
                Reason::RegistryUnreadable,
                Some("the key registry cannot be read"),
            );
            json!({"name": "signing", "required": false, "state": "red", "note": why,
                "recovery": "a registry key signs a version that fixes the named line; for a removed key enrolled again, bay enrol --remove <class> <name> --reason revoked"})
        }
    };
    report.rows.push(row);
}

/// For a stated executor under a registry (G63, § 10a): the executor key that signs its
/// publications and the producer key that signs its evidence are set, and each one's name is
/// enrolled in its class file. Without a registry the executor publishes unsigned: green. Under
/// `[policy] signing = "approvals"` (N-21) neither key is needed: green.
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
        // N-21: under `[policy] signing = "approvals"` no executor or producer key signs.
        Ok(Some(registry)) if !registry.signs_all() => {
            report.rows.push(json!({"name": "executor-keys", "required": false,
                "state": "green", "signing": registry.mode().name(),
                "note": "the policy at trunk says [policy] signing = \"approvals\": the executor publishes unsigned, and only approvals are signed"}));
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

/// G67, for a stated executor: its own presence as its last pass wrote it (the local
/// `refs/bay/executor/<executor>`, `crate::presence`), with its age on this box's clock, whether
/// it is stale, and how its last push went. Never red: an executor that has not run yet has
/// published none, and its first pass publishes it.
fn presence_row(git: &Git, config: &Config, keys: &crate::signing::Keys, report: &mut Report) {
    let name = crate::presence::presence_ref(&keys.executor_name);
    let record = git
        .ref_target(&name)
        .ok()
        .flatten()
        .and_then(|tip| crate::presence::record_at(git, &tip).ok().flatten());
    let mut row = json!({"name": "presence", "required": false,
        "executor": keys.executor_name, "ref": name});
    match record {
        None => {
            row["state"] = json!("none");
            row["note"] = json!(
                "this executor has published no presence here yet; each pass of bay runner run publishes it"
            );
        }
        Some(record) => {
            // One box's own record: its time and this clock are the same machine's.
            let at = crate::live::unix_at(&record).unwrap_or(0);
            let judged = crate::presence::age(
                &record,
                at,
                crate::presence::reader_now(),
                config.takeover().as_secs(),
                config.progress_deadline().as_secs(),
            );
            row["state"] = json!(if judged.stale { "stale" } else { "green" });
            row["at"] = record["at"].clone();
            row["age_s"] = json!(judged.age_s);
            row["stale_after_s"] = json!(judged.stale_after_s);
            row["bay"] = record["bay"].clone();
            row["environments"] = record["environments"].clone();
            row["holding"] = record["holding"].clone();
            row["queue_depth"] = record["queue_depth"].clone();
        }
    }
    let common = git
        .run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .ok();
    if let Some(last) = common
        .as_deref()
        .and_then(|common| crate::presence::last_push(Path::new(common)))
    {
        row["last_push"] = last;
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

/// G285: the `check-read-paths` row: the box's read-only paths for checks
/// (`bay.checkReadPath`), each with whether it exists. A path that does not exist, or a value
/// that is not absolute (every check is then refused, naming it), is degraded. No row when
/// the box sets none.
fn read_paths_row(git: &Git, report: &mut Report) {
    let key = crate::check::READ_PATH_KEY;
    let row = match crate::check::read_paths(git) {
        Ok(paths) if paths.is_empty() => return,
        Ok(paths) => {
            let missing: Vec<String> = paths
                .iter()
                .filter(|path| !path.exists())
                .map(|path| path.display().to_string())
                .collect();
            let listed: Vec<Value> = paths
                .iter()
                .map(|path| json!({"path": path.display().to_string(), "exists": path.exists()}))
                .collect();
            let mut row = json!({"name": "check-read-paths", "key": key, "paths": listed,
                "state": if missing.is_empty() { "green" } else { "degraded" }});
            if !missing.is_empty() {
                row["missing"] = json!(missing);
                row["note"] =
                    json!("a read path that does not exist is not bound: a check cannot read it");
                row["recovery"] = json!(format!(
                    "create the path, or remove it: git config --global --unset {key} <path>"
                ));
            }
            row
        }
        Err(why) => json!({"name": "check-read-paths", "key": key, "state": "degraded",
            "note": why,
            "recovery": format!("set each value to an absolute path or one starting with ~/: git config --global --replace-all {key} <path>")}),
    };
    report.rows.push(row);
}

/// The installed skill at `$HOME/.claude/skills/bay/` (or `$CLAUDE_CONFIG_DIR/skills/bay/`
/// when that is set, the path install writes, G65) against the skill this bay version
/// installs (§ 15.2, G49), and (G319) the copy in every other Claude config directory on the
/// machine (`skill::config_dirs`), each listed in `dirs`. A consumer repository's skills are
/// never this copy. A mismatch is red on every box; a missing copy is red only where the Claude
/// adapter is configured. `mismatched` names each config directory that makes the row red.
fn skill_row(git: &Git, config: &Config, report: &mut Report) -> Result<(), String> {
    let home =
        crate::platform::home().ok_or("HOME is not set; doctor cannot name the skill path")?;
    let path = skill::installed_path(Path::new(&home), skill::config_dir().as_deref());
    let required = config.claude_adapter();
    let mut dirs = Vec::new();
    let mut mismatched = Vec::new();
    let (mut missing, mut differs) = (false, false);
    for dir in skill::config_dirs(Path::new(&home), git) {
        let state = match skill::compare(&dir.join("skills").join("bay"))? {
            skill::Installed::Matches => "green",
            skill::Installed::Missing => {
                missing = true;
                if required {
                    mismatched.push(dir.display().to_string());
                }
                "missing"
            }
            skill::Installed::Differs => {
                differs = true;
                mismatched.push(dir.display().to_string());
                "differs"
            }
        };
        dirs.push(json!({"dir": dir.display().to_string(), "state": state}));
    }
    let mut row = json!({"name": "skill", "path": path.display().to_string(),
        "required": required, "dirs": dirs});
    if mismatched.is_empty() {
        row["state"] = json!(if missing { "missing" } else { "green" });
    } else {
        let note = if differs {
            "skill does not match this bay"
        } else {
            "skill not installed"
        };
        row["state"] = json!("red");
        row["note"] = json!(note);
        row["mismatched"] = json!(mismatched);
        row["recovery"] = json!("run install again");
        report.red(Reason::SkillMismatch, Some(note));
    }
    report.rows.push(row);
    Ok(())
}

fn probe_claude() -> Result<Program, String> {
    probe_program("claude")
}

/// `<name> --version` on this box: the version it prints (Codex prints `codex-cli 0.153.2`),
/// or absent when no such program is on `PATH`.
fn probe_program(name: &str) -> Result<Program, String> {
    let mut cmd = Command::new(crate::platform::program(name));
    cmd.arg("--version");
    let display = format!("{name} --version");
    match git::run_bounded(cmd, &display) {
        Err(GitError::Spawn(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(Program::Absent)
        }
        Err(error) => Err(error.message()),
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            Version::find(&text)
                .map(Program::Found)
                .ok_or_else(|| format!("{display} printed no version: {}", text.trim()))
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
    claude: &Program,
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
        Program::NotConfigured => "claude adapter not configured".to_owned(),
        Program::Absent => format!("claude absent ({CLAUDE_RECOVERY})"),
        Program::Found(version) if version < minimum => {
            format!("claude {version} (floor {CLAUDE_HOOK_MINIMUM}, {CLAUDE_RECOVERY})")
        }
        Program::Found(version) => format!("claude {version}"),
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

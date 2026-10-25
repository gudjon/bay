//! `.bay/config.toml` in the repository (`INTERFACES` § 7).
//!
//! Every key § 7 lists is parsed and typed, so a mistyped one is a config error. Retired keys
//! (`[check] command`, `[update]`) are accepted and ignored, and doctor names them
//! ([`Config::ignored`]). `[plugin.runtime] command` (the runtime adapter, `INTERFACES` § 4) is
//! not in § 7's list yet. `[runner]` and `[retention]` are read by built features but are
//! not in § 7's list yet (BACKLOG 18), nor is `[view]` (G175), nor `[red]` (NEW-6), nor are
//! `[[target]]` and `[[hold]]` (NEW-5, NEW-9, `crate::target`), nor `[plugin.hold]` (N-8,
//! `crate::hold`), nor `[[lane]]` (NEW-4, [`Lane`]), nor `[plugin.prune]` (N-19, `crate::heads`). A
//! key or table bay does not know is a config error that names it, as is `evidence_reuse` set to anything but `"off"` or `"exact-inputs"`: a misspelled
//! setting never loads as a silent default. A config written for a later bay is refused by
//! this one, and the recovery is `bay update`.

use std::path::Path;

use serde::Deserialize;

pub const CONFIG_PATH: &str = ".bay/config.toml";

/// The values of `[policy] evidence_reuse` (`INTERFACES` § 7).
const EVIDENCE_REUSE: [&str; 2] = ["off", "exact-inputs"];

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `main = "<branch>"` (BACKLOG 55: the branch bay serves, any name; `trunk` before 0.3.3).
    #[serde(rename = "main")]
    pub trunk: Option<String>,
    /// `[[target]]`: the promotion targets (NEW-5, `crate::target`). A promotion moves a ref
    /// such as `refs/heads/staging` to a commit already on its source; it is not a landing.
    #[serde(default, rename = "target")]
    pub targets: Vec<Target>,
    /// `[[hold]]` (0.3.0; `[[hold]]` before): the hold windows of main itself (N-8,
    /// `crate::hold`) and of the targets (NEW-9, `crate::target`). One with no `target` holds
    /// main.
    #[serde(default, rename = "hold")]
    pub freezes: Vec<Freeze>,
    pub remote: Option<String>,
    pub agent: Option<Agent>,
    pub trailers: Option<Trailers>,
    /// `[runner]` (0.3.0; `[runner]` before): the runner that finishes operations.
    #[serde(rename = "runner")]
    pub executor: Option<Executor>,
    pub retention: Option<Retention>,
    /// `[check]` is retired (ruling Q14): checks are declared in `.bay/policy.toml`, whose
    /// `[[check]]` entries name their own commands, so the command a check runs has one
    /// definition, inside the policy's revision and digest (G141). An older config that still
    /// carries the table keeps loading; nothing reads it but [`Config::ignored`].
    check: Option<toml::Table>,
    /// `[plugin.<kind>]` (0.3.0): each plugin's `command` and `timeout_s`, one shape for every
    /// kind ([`Plugins`]).
    #[serde(default)]
    pub plugin: Plugins,
    pub policy: Option<PolicyProfile>,
    pub hooks: Option<Hooks>,
    pub view: Option<View>,
    pub red: Option<Red>,
    /// `[[lane]]` (NEW-4): the sandbox lanes, whose changes run their checks and never land.
    #[serde(default, rename = "lane")]
    pub lanes: Vec<Lane>,
    /// `[update]` is accepted and ignored: the update pin and switch are the box's
    /// (`git config --global bay.update*`, `DESIGN` § 15.6), never a repository's, and an older
    /// config that still carries the table must keep loading.
    update: Option<toml::Table>,
}

/// One `[[target]]` (NEW-5): a ref bay judges promotions to, never one it lands on.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// The target's name, an operator name (`staging`, `production`).
    pub name: String,
    /// The ref it names; `refs/heads/<name>` unless set.
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    /// What it is promoted from: the trunk (the default, or the trunk's name), or another
    /// target's name.
    pub from: Option<String>,
    /// The check ids of `.bay/policy.toml` whose evidence must be green for a commit the
    /// target moves to: post checks, or checks run only for this target.
    #[serde(default)]
    pub requires: Vec<String>,
    /// The minimum age, in seconds of the commit's committer time, before a promotion.
    pub soak_s: Option<u64>,
    /// N-7: whether each executor pass fast-forwards the target to the newest commit that is
    /// promotable to it (`crate::target::advance`); off unless set. Only a target that
    /// `requires` checks may advance.
    #[serde(default)]
    pub advance: bool,
}

impl Target {
    /// The ref this target names.
    #[must_use]
    pub fn reference(&self) -> String {
        self.reference
            .clone()
            .unwrap_or_else(|| format!("refs/heads/{}", self.name))
    }

    /// The target this one is promoted from, or `None` for the trunk.
    #[must_use]
    pub fn from_target(&self, trunk: &str) -> Option<&str> {
        self.from.as_deref().filter(|from| *from != trunk)
    }

    /// The soak in seconds; none unless set.
    #[must_use]
    pub fn soak(&self) -> u64 {
        self.soak_s.unwrap_or(0)
    }
}

/// One `[[hold]]` (NEW-9, N-8): until `end`, its target takes only a commit whose merge carries
/// `Fixes:` or `Reverts:`. With no `target`, or main's name, it holds main (`crate::hold`):
/// until `end`, only such a merge is published. [`Config::parse`] fills a missing target with
/// main's name.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Freeze {
    #[serde(default)]
    pub target: String,
    /// The end, in RFC 3339 UTC to the second (`2026-12-24T00:00:00Z`).
    pub end: String,
    pub reason: String,
}

impl Freeze {
    /// Whether the freeze is in force at Unix time `now`. An end bay cannot read never loads
    /// ([`Config::parse`]), so here it reads as in force.
    #[must_use]
    pub fn active(&self, now: u64) -> bool {
        crate::clock::unix_of(&self.end).is_none_or(|end| now < end)
    }
}

/// `[plugin.<kind>]` (0.3.0): the plugins, each a program path relative to the work tree, then
/// its arguments (`crate::run`), with a bound of its own (`timeout_s`, the kind's default unless
/// set):
/// - `notify` (`INTERFACES` § 4, `crate::notify`): called once after each event is written;
/// - `wake` (G58, `crate::talk::wake`): asked once when this machine accepts a talk;
/// - `hold` (N-8, `crate::hold`): run as `<command> status` before each publication;
/// - `prune` (N-19, `crate::heads`): after each branch `bay prune` deletes, run, materialized
///   from main's tip, as `<command> pruned <branch> <sha> <archive ref>`. It is where a team
///   reaches outside the repository, to close the branch's open pull request on its host for
///   example; bay itself never does;
/// - `runtime` (`INTERFACES` § 4, `crate::runtime`): asked by `bay status` for `running` and
///   `effective` records about a merged commit;
/// - `assets` (BACKLOG 49, `crate::assets`): with `over_limit = "offload"`, keeps a file over
///   `[limits] max_file_mib` out of Git, `put` at `bay save` and named in `.bay-assets`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugins {
    pub notify: Option<Plugin>,
    pub wake: Option<Plugin>,
    pub hold: Option<Plugin>,
    pub prune: Option<Plugin>,
    pub runtime: Option<Plugin>,
    pub assets: Option<AssetsPlugin>,
}

impl Plugins {
    /// Each plugin's command is not empty and its bound is at least 1 s; the hold and prune
    /// plugins keep their own longest bounds.
    fn validate(&self) -> Result<(), String> {
        // An empty assets command names the remote's own store (66 E), so it is no empty command.
        let assets = self.assets.as_ref().map(|assets| Plugin {
            command: None,
            timeout_s: assets.timeout_s,
        });
        for (kind, plugin) in [
            ("notify", &self.notify),
            ("wake", &self.wake),
            ("hold", &self.hold),
            ("prune", &self.prune),
            ("runtime", &self.runtime),
            ("assets", &assets),
        ] {
            let Some(plugin) = plugin else { continue };
            if plugin
                .command
                .as_deref()
                .is_some_and(|command| command.trim().is_empty())
            {
                return Err(format!("[plugin.{kind}] command is empty"));
            }
            if plugin.timeout_s == Some(0) {
                return Err(format!(
                    "[plugin.{kind}] timeout_s = 0: a plugin's bound is at least 1 s"
                ));
            }
        }
        // A hold is asked before each publication: its bound stays short.
        if let Some(timeout) = self.hold.as_ref().and_then(|hold| hold.timeout_s)
            && !(1..=MAX_HOLD_TIMEOUT_S).contains(&timeout)
        {
            return Err(format!(
                "[plugin.hold] timeout_s = {timeout}: the hold plugin's bound is 1 to \
                 {MAX_HOLD_TIMEOUT_S} s"
            ));
        }
        if let Some(timeout) = self.prune.as_ref().and_then(|prune| prune.timeout_s)
            && !(1..=MAX_HEADS_TIMEOUT_S).contains(&timeout)
        {
            return Err(format!(
                "[plugin.prune] timeout_s = {timeout}: the prune plugin's bound is 1 to \
                 {MAX_HEADS_TIMEOUT_S} s"
            ));
        }
        Ok(())
    }
}

/// `[plugin.assets]` (BACKLOG 49): the plugin that stores a file's bytes outside Git, and what
/// bay does with a file over `[limits] max_file_mib`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetsPlugin {
    /// The store's program; empty (or left out): the remote's own store (BACKLOG 66 E).
    #[serde(default)]
    pub command: String,
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub over_limit: OverLimit,
    /// BACKLOG 62: globs of files the plugin stores whatever their size, so their bytes can be
    /// purged later while history keeps their address (`offload = ["**/*.pdf"]`).
    #[serde(default)]
    pub offload: Vec<String>,
}

/// What bay does with a file over `[limits] max_file_mib`.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OverLimit {
    /// Refused `CHANGE_TOO_LARGE`, as with no plugin.
    #[default]
    Refuse,
    /// Stored by the plugin and named in `.bay-assets`.
    Offload,
}

/// One `[plugin.<kind>]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    pub command: Option<String>,
    pub timeout_s: Option<u64>,
}

impl Plugin {
    /// Its bound: `timeout_s`, else the kind's `default`.
    #[must_use]
    pub fn timeout(&self, default: std::time::Duration) -> std::time::Duration {
        self.timeout_s
            .map_or(default, std::time::Duration::from_secs)
    }
}

/// The longest `[plugin.prune] timeout_s` (N-19): the prune plugin runs once per pruned branch.
pub const MAX_HEADS_TIMEOUT_S: u64 = 600;

/// `[policy]`: the completion profile and evidence reuse ([`Config::evidence_reuse`]).
/// `profile` names the `[profile.<name>]` of `.bay/policy.toml` that `bay status` judges
/// completion by (`crate::completion`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyProfile {
    pub profile: Option<String>,
    pub evidence_reuse: Option<String>,
}

/// The `[agent] product` that selects the Claude Code adapter (`DESIGN` § 15.2).
pub const CLAUDE_PRODUCT: &str = "claude";

/// The `[agent] product` that names the Codex adapter (G308).
pub const CODEX_PRODUCT: &str = "codex";

/// `[hooks] cache_max_age_s` (`DESIGN` § 11): the coordination cache's maximum age for
/// `tool.before` (`crate::coord`). Default 30 (`INTERFACES` § 7). `[hooks] notice_budget_bytes`
/// (ruling R6): the bytes of notices a session is told per hour (`crate::notice`). Default 2048.
/// `[hooks] checkpoint_idle_s` (G165): how long after its last checkpoint a session's next hook
/// event checkpoints its view (`crate::session`). Default 600.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    pub cache_max_age_s: Option<u64>,
    pub notice_budget_bytes: Option<u64>,
    pub checkpoint_idle_s: Option<u64>,
}

/// `[view] warm_history` (G175): whether `bay start` and `start --resume` also fetch the history
/// of the view's declared paths (`crate::view::warm_history`). Off by default: a view costs what
/// the task reads (G18), and history is read only by a task that asks for it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub warm_history: Option<bool>,
}

/// `[red] window` (NEW-6): how many first-parent trunk commits a red judgement (G244) and the
/// unattributed scan (G85) read back. Default [`crate::red::DEFAULT_WINDOW`]; at most
/// [`MAX_RED_WINDOW`]. Read from the config committed at trunk, so the window is the landed
/// one. A post check no commit in the window answers is pending, never green: a trunk that
/// lands faster than its post checks answer needs a window as deep as their lag.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Red {
    pub window: Option<usize>,
}

/// The deepest `[red] window`: each commit in it may cost one read of its evidence notes.
pub const MAX_RED_WINDOW: usize = 10_000;

/// The longest `[plugin.hold] timeout_s` (N-8): the hold adapter is asked before each publication.
pub const MAX_HOLD_TIMEOUT_S: u64 = 120;

/// How long a closed change keeps its drafts before the retention cut (§ 5), and the bay ref
/// count past which an executor compacts markers (§ 10a ref-growth budget, `crate::compact`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    pub window_s: Option<u64>,
    pub ref_budget: Option<u64>,
}

/// `[[lane]]` (NEW-4): a sandbox lane. `refs` are the ref names its work lives under, as the
/// remote names them (`refs/heads/research/*`); each is a full ref name under `refs/`, outside
/// `refs/bay/`, with at most one `*`, which matches any run of characters, slashes too. A change
/// started with `bay start --lane <name>` runs its checks and never lands (its run ends `ok`); a
/// change of no lane whose view reaches a lane ref's commits off trunk is refused (`LANE_LEAK`).
/// `window_s` is how long an ended lane change keeps its view, shorter than the retention
/// window (`[retention] window_s`); by default [`DEFAULT_LANE_WINDOW_S`], or the retention
/// window when that is shorter.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lane {
    pub name: String,
    pub refs: Vec<String>,
    pub window_s: Option<u64>,
}

/// A lane's retention window when it names none: one day.
pub const DEFAULT_LANE_WINDOW_S: u64 = 24 * 3600;

/// An external executor implementing the contract of `INTERFACES` § 3a.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Executor {
    pub command: Option<String>,
    /// How long the executor's `submit` may take before the submission ends as timed out.
    pub submission_timeout_s: Option<u64>,
    /// The pre-publication execution budget, from the start of the checks.
    pub execution_budget_s: Option<u64>,
    /// The executor lease (§ 9a): an owner whose chain head does not change for this long, on
    /// an observer's monotonic clock, may be replaced. Default 60.
    pub lease_s: Option<u64>,
    /// How often an owner renews its lease with a heartbeat commit. Default 10.
    pub heartbeat_s: Option<u64>,
    /// The takeover window for adopting another actor's idle change: longer than the lease.
    /// Default twice the lease.
    pub takeover_s: Option<u64>,
    /// The publish window added to the execution budget for a queue head's progress deadline.
    pub publish_window_s: Option<u64>,
    /// Culprit finding (`crate::bisect`): when a post check is red over more than one landing,
    /// an executor that serves its environment runs it on the range's midpoints. Default off.
    pub bisect_post: Option<bool>,
    /// Never a repository's setting (ruling R14): whether checks run with no sandbox is the
    /// box's choice, global Git config `bay.runnerSandbox = none`. Read only to refuse it.
    pub sandbox: Option<toml::Value>,
    /// NEW-2 (`crate::throughput`): how many queue entries behind the head an executor that is
    /// not running the head checks ahead of their turn, each on the base the entries before it
    /// predict. Default 0, off; at most [`crate::throughput::MAX_AHEAD`].
    pub ahead: Option<usize>,
    /// NEW-10 (`crate::throughput`): an entry whose footprint is disjoint from every entry's
    /// ahead of it is checked ahead of its turn on the current tip, and a record of a check with
    /// `footprint = true` is carried onto a later candidate whose read set is the same. Default
    /// off.
    pub footprint_lanes: Option<bool>,
    /// Batching with bisection (`crate::throughput`): `batch = { min, max, max_failure_rate }`.
    /// Absent, off.
    pub batch: Option<Batch>,
    /// N-17 (`crate::check::run_all`): how many of a candidate's checks run at once, each in
    /// its own work directory. Default 1, one after another; at most
    /// [`crate::check::MAX_PARALLEL_CHECKS`].
    pub parallel_checks: Option<usize>,
}

/// `[runner] batch` (`crate::throughput`): at least `min` members (default 1; a batch always
/// has two), at most `max` (default [`crate::throughput::DEFAULT_BATCH_MAX`]), and none while the
/// lane's recent failure rate is above `max_failure_rate` (default
/// [`crate::throughput::DEFAULT_MAX_FAILURE_RATE`]).
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub min: Option<usize>,
    pub max: Option<usize>,
    pub max_failure_rate: Option<f64>,
}

/// The configurable trailer keys. `Change-Id`, `Revision-Id`, `Request-Id`, `Candidate-Id`,
/// `Registry-Id`, `Bay-Epoch`, `Bay-Session`, `Bay-Batch` and `Bay-After` are fixed names. The
/// actor trailer carries the actor's bare name; its session goes in `Bay-Session`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trailers {
    pub intent: Option<String>,
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub product: String,
    /// The instruction files a session's tree is checked against trunk for at `SessionStart`
    /// (G155); default [`DEFAULT_INSTRUCTIONS`].
    pub instructions: Option<Vec<String>>,
}

/// The [`crate::memo`] kind of the config text committed at a commit: `Some(text)`, or `None`
/// when that commit's tree has no config. [`Config::at`] and the trunk read when a repository
/// opens share it.
pub(crate) const COMMITTED: &str = "config";

/// The instruction files of `[agent] instructions` when the config names none (G155).
pub const DEFAULT_INSTRUCTIONS: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

impl Config {
    /// Reads `<root>/.bay/config.toml`. A missing file is the default config.
    ///
    /// # Errors
    ///
    /// The file exists but cannot be read or parsed.
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = root.join(CONFIG_PATH);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).map_err(|error| format!("{}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    /// Reads the work tree's config, or, when the work tree has none (a machine cache cloned
    /// with `--no-checkout`), the config committed at `HEAD`.
    ///
    /// # Errors
    ///
    /// A config that exists but does not parse, or one `HEAD`'s tree names that cannot be read
    /// (G104b): only a config `HEAD` lacks is the default.
    pub fn load_or_committed(root: &Path, git: &crate::git::Git) -> Result<Self, String> {
        if root.join(CONFIG_PATH).exists() {
            return Self::load(root);
        }
        match git.file_at("HEAD", CONFIG_PATH) {
            Ok(Some(text)) => {
                Self::parse(&text).map_err(|error| format!("HEAD:{CONFIG_PATH}: {error}"))
            }
            Ok(None) => Ok(Self::default()),
            Err(error) => Err(error.message()),
        }
    }

    /// The config committed at `commit`; the default when that commit has none.
    ///
    /// # Errors
    ///
    /// The committed config does not parse, or git cannot say whether it exists.
    pub fn at(git: &crate::git::Git, commit: &str) -> Result<Self, String> {
        let path = format!("{commit}:{CONFIG_PATH}");
        // The config text at a commit never changes: read once per process (plan B2).
        let text = crate::memo::at(
            COMMITTED,
            git,
            commit,
            Result::is_ok,
            || -> Result<Option<String>, String> {
                // The tree names the file; its blob need not be in a blob-less store (G104b).
                let exists = git
                    .run(&["rev-parse", "--verify", "-q", &path])
                    .map(|_| true)
                    .or_else(|_| {
                        git.run(&["rev-parse", "--verify", &format!("{commit}^{{commit}}")])
                            .map(|_| false)
                            .map_err(|error| error.message())
                    })?;
                if !exists {
                    return Ok(None);
                }
                git.run(&["cat-file", "blob", &path])
                    .map(Some)
                    .map_err(|error| error.message())
            },
        )?;
        match text {
            Some(text) => Self::parse(&text).map_err(|error| format!("{path}: {error}")),
            None => Ok(Self::default()),
        }
    }

    /// Parses a config text. A key bay does not know, a value of the wrong type, and a value
    /// this bay does not build are each an error that names it, never a silent default.
    ///
    /// # Errors
    ///
    /// The text is not a config this bay can honour.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut config: Self = toml::from_str(text).map_err(|error| error.to_string())?;
        // A `[[hold]]` with no target holds main.
        let main = config.trunk().to_owned();
        for hold in &mut config.freezes {
            if hold.target.is_empty() {
                hold.target.clone_from(&main);
            }
        }
        if let Some(reuse) = config
            .policy
            .as_ref()
            .and_then(|policy| policy.evidence_reuse.as_deref())
            && !EVIDENCE_REUSE.contains(&reuse)
        {
            return Err(format!(
                "[policy] evidence_reuse = \"{reuse}\": the values are \"off\" and \
                 \"exact-inputs\""
            ));
        }
        // R14: a repository never chooses to run checks unconfined, so a committed config
        // cannot lower the isolation of every later check.
        if config
            .executor
            .as_ref()
            .is_some_and(|executor| executor.sandbox.is_some())
        {
            return Err(
                "[runner] sandbox is not a repository setting (ruling R14): running \
                 checks with no sandbox is the box's choice, git config --global \
                 bay.runnerSandbox none"
                    .to_owned(),
            );
        }
        // NEW-6: an empty window would judge nothing, and read every post check as pending.
        if let Some(window) = config.red.as_ref().and_then(|red| red.window)
            && !(1..=MAX_RED_WINDOW).contains(&window)
        {
            return Err(format!(
                "[red] window = {window}: the window is 1 to {MAX_RED_WINDOW} main commits"
            ));
        }
        config.validate_targets()?;
        config.plugin.validate()?;
        config.validate_lanes()?;
        config.validate_throughput()?;
        let parallel = config.parallel_checks();
        if !(1..=crate::check::MAX_PARALLEL_CHECKS).contains(&parallel) {
            return Err(format!(
                "[runner] parallel_checks = {parallel}: 1 to {} of a candidate's checks run at \
                 once",
                crate::check::MAX_PARALLEL_CHECKS
            ));
        }
        // § 4: the takeover window is longer than the executor lease.
        if config.takeover() <= config.lease() {
            return Err(format!(
                "[runner] takeover_s = {}: the takeover window must be longer than the lease \
                 (lease_s = {})",
                config.takeover().as_secs(),
                config.lease().as_secs()
            ));
        }
        Ok(config)
    }

    /// The throughput levers (`crate::throughput`): each in its bounds, and each a rule of
    /// evidence reuse, so none is on with `[policy] evidence_reuse = "off"`.
    fn validate_throughput(&self) -> Result<(), String> {
        let ahead = self.ahead();
        if ahead > crate::throughput::MAX_AHEAD {
            return Err(format!(
                "[runner] ahead = {ahead}: at most {} entries are checked ahead of their turn",
                crate::throughput::MAX_AHEAD
            ));
        }
        if let Some(batch) = self.batch() {
            if batch.min < 1 || batch.min > batch.max || batch.max > crate::throughput::MAX_BATCH {
                return Err(format!(
                    "[runner] batch = {{ min = {}, max = {} }}: 1 <= min <= max <= {}",
                    batch.min,
                    batch.max,
                    crate::throughput::MAX_BATCH
                ));
            }
            if !(0.0..=1.0).contains(&batch.max_failure_rate) {
                return Err(format!(
                    "[runner] batch max_failure_rate = {}: a rate is 0 to 1",
                    batch.max_failure_rate
                ));
            }
        }
        let on = self.throughput_levers();
        if !on.is_empty() && self.reuse_setting() == Some("off") {
            return Err(format!(
                "[runner] {}: each throughput lever counts evidence only through reuse, and \
                 [policy] evidence_reuse = \"off\" turns reuse off; drop that line",
                on.join(", ")
            ));
        }
        Ok(())
    }

    /// The throughput levers that are on (`crate::throughput`): each one counts evidence only
    /// through reuse, so it turns reuse on ([`Config::evidence_reuse`]).
    fn throughput_levers(&self) -> Vec<&'static str> {
        [
            (self.ahead() > 0, "ahead"),
            (self.footprint_lanes(), "footprint_lanes"),
            (self.batch().is_some(), "batch"),
        ]
        .into_iter()
        .filter_map(|(on, key)| on.then_some(key))
        .collect()
    }

    fn reuse_setting(&self) -> Option<&str> {
        self.policy
            .as_ref()
            .and_then(|policy| policy.evidence_reuse.as_deref())
    }

    /// NEW-5, NEW-9: each target has an operator name, used once and never the trunk's; a ref
    /// of its own outside bay's namespaces and never the trunk; a source that is the trunk or
    /// another target, with no cycle; and named checks. Each freeze names a target or the
    /// trunk (N-8), an end bay can read and a reason.
    fn validate_targets(&self) -> Result<(), String> {
        let trunk = self.trunk();
        let trunk_ref = format!("refs/heads/{trunk}");
        for (at, target) in self.targets.iter().enumerate() {
            let named = |why: String| format!("[[target]] {:?}: {why}", target.name);
            crate::names::operator_name(&target.name).map_err(named)?;
            if target.name == trunk {
                return Err(named(format!(
                    "a target is never named as main ({trunk:?})"
                )));
            }
            let reference = target.reference();
            if !is_target_ref(&reference) || reference == trunk_ref {
                return Err(named(format!(
                    "ref = {reference:?}: a target's ref is a full ref name under refs/, \
                     never main's and never under {} or {}",
                    crate::names::BAY,
                    crate::names::NOTES
                )));
            }
            if let Some(other) = self.targets[..at]
                .iter()
                .find(|other| other.name == target.name || other.reference() == reference)
            {
                return Err(named(format!(
                    "two [[target]] entries name {:?} or {reference:?}",
                    other.name
                )));
            }
            if let Some(from) = target.from_target(trunk)
                && !self.targets.iter().any(|other| other.name == from)
            {
                return Err(named(format!(
                    "from = {from:?} is neither main ({trunk:?}) nor a [[target]]"
                )));
            }
            if target.requires.iter().any(String::is_empty) {
                return Err(named("requires names an empty check id".to_owned()));
            }
            if target.advance && target.requires.is_empty() {
                return Err(named(
                    "advance = true needs requires: with no required check, the ref the \
                     runner advances would name the last commit, never the last green one"
                        .to_owned(),
                ));
            }
            // A source chain longer than the targets has a cycle.
            let mut source = target;
            for _ in 0..=self.targets.len() {
                match source
                    .from_target(trunk)
                    .and_then(|from| self.targets.iter().find(|other| other.name == from))
                {
                    Some(next) => source = next,
                    None => break,
                }
            }
            if source.from_target(trunk).is_some() {
                return Err(named(
                    "its from chain never reaches main: a cycle".to_owned(),
                ));
            }
        }
        for freeze in &self.freezes {
            // N-8: a freeze of the trunk itself holds the trunk (`crate::hold`).
            if freeze.target != trunk
                && !self
                    .targets
                    .iter()
                    .any(|target| target.name == freeze.target)
            {
                return Err(format!(
                    "[[hold]] target = {:?} names neither main ({trunk:?}) nor a [[target]]",
                    freeze.target
                ));
            }
            if crate::clock::unix_of(&freeze.end).is_none() {
                return Err(format!(
                    "[[hold]] of {:?}: end = {:?} is not an RFC 3339 UTC time to the second \
                     (2026-12-24T00:00:00Z)",
                    freeze.target, freeze.end
                ));
            }
            if freeze.reason.trim().is_empty() {
                return Err(format!("[[hold]] of {:?} has no reason", freeze.target));
            }
        }
        Ok(())
    }

    /// NEW-4: each `[[lane]]` has a name of its own, short and lower-case, at least one ref
    /// pattern, no pattern that could name trunk or a bay ref, and a window shorter than the
    /// retention window.
    fn validate_lanes(&self) -> Result<(), String> {
        let trunk = format!("refs/heads/{}", self.trunk());
        let retention = self.retention_window().as_secs();
        for (at, lane) in self.lanes.iter().enumerate() {
            let name = &lane.name;
            let named = !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
            if !named {
                return Err(format!(
                    "[[lane]] name = \"{name}\": a lane name is 1 to 64 of a-z, 0-9 and -"
                ));
            }
            if self.lanes[..at].iter().any(|other| other.name == *name) {
                return Err(format!("[[lane]] name = \"{name}\" is declared twice"));
            }
            if lane.refs.is_empty() {
                return Err(format!("[[lane]] {name}: refs lists no ref pattern"));
            }
            for pattern in &lane.refs {
                let shaped = pattern.starts_with("refs/")
                    && !pattern.starts_with(crate::names::BAY)
                    && pattern.matches('*').count() <= 1
                    && !pattern.ends_with('/')
                    && !pattern.chars().any(|ch| {
                        ch.is_whitespace() || matches!(ch, ':' | '+' | '^' | '~' | '?' | '[' | '\\')
                    });
                if !shaped {
                    return Err(format!(
                        "[[lane]] {name}: ref pattern \"{pattern}\" is not a full ref name under refs/, outside {}, with at most one *",
                        crate::names::BAY
                    ));
                }
                if crate::lane::matches(pattern, &trunk) {
                    return Err(format!(
                        "[[lane]] {name}: ref pattern \"{pattern}\" matches main ({trunk})"
                    ));
                }
            }
            if let Some(window) = lane.window_s
                && window >= retention
            {
                return Err(format!(
                    "[[lane]] {name}: window_s = {window} must be shorter than the retention window ([retention] window_s = {retention})"
                ));
            }
        }
        Ok(())
    }

    /// The `[[target]]` named `name`.
    #[must_use]
    pub fn target(&self, name: &str) -> Option<&Target> {
        self.targets.iter().find(|target| target.name == name)
    }

    /// The `[[lane]]` named `name`.
    #[must_use]
    pub fn lane(&self, name: &str) -> Option<&Lane> {
        self.lanes.iter().find(|lane| lane.name == name)
    }

    /// How long an ended change of `lane` keeps its view and journal (NEW-4): the lane's
    /// `window_s`, else [`DEFAULT_LANE_WINDOW_S`] or the retention window, whichever is shorter.
    #[must_use]
    pub fn lane_window(&self, lane: &Lane) -> std::time::Duration {
        lane.window_s.map_or_else(
            || {
                self.retention_window()
                    .min(std::time::Duration::from_secs(DEFAULT_LANE_WINDOW_S))
            },
            std::time::Duration::from_secs,
        )
    }

    /// Whether `[policy] evidence_reuse = "exact-inputs"`: a producer-signed record whose tree,
    /// base tree, commit (for a commit-keyed check), check, environment, manifest, revision,
    /// provenance and fixtures all match stands for a new run (`DESIGN` § 7, G7). Off by
    /// default; for a change with named inputs nothing is reused, and without a registry that
    /// signs everything only when the policy at the base says `[policy] reuse_unsigned = true`
    /// (G318), where the record stands unsigned.
    #[must_use]
    pub fn evidence_reuse(&self) -> bool {
        // 0.3.0: a throughput lever implies reuse; the key is needed only for reuse on its own.
        match self.reuse_setting() {
            Some("exact-inputs") => true,
            Some(_) => false,
            None => !self.throughput_levers().is_empty(),
        }
    }

    /// The coordination cache's maximum age for `tool.before`: older, a write to a declared input
    /// gets `COVERAGE_UNKNOWN` (`DESIGN` § 11).
    #[must_use]
    pub fn cache_max_age(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.hooks
                .as_ref()
                .and_then(|hooks| hooks.cache_max_age_s)
                .unwrap_or(30),
        )
    }

    /// The completion profile `[policy] profile` selects; `default` unless configured.
    #[must_use]
    pub fn profile(&self) -> &str {
        self.policy
            .as_ref()
            .and_then(|policy| policy.profile.as_deref())
            .unwrap_or(crate::completion::DEFAULT_PROFILE)
    }

    /// The bytes of notices one session is told per hour (ruling R6); past it, a notice is
    /// counted, not emitted.
    #[must_use]
    pub fn notice_budget(&self) -> u64 {
        self.hooks
            .as_ref()
            .and_then(|hooks| hooks.notice_budget_bytes)
            .unwrap_or(2048)
    }

    /// How long after a session's last checkpoint its next hook event checkpoints its view
    /// (G165): a killed session loses at most this much work.
    #[must_use]
    pub fn checkpoint_idle(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.hooks
                .as_ref()
                .and_then(|hooks| hooks.checkpoint_idle_s)
                .unwrap_or(600),
        )
    }

    /// Whether a view warms the history of its declared paths (`[view] warm_history`, G175).
    #[must_use]
    pub fn warm_history(&self) -> bool {
        self.view
            .as_ref()
            .and_then(|view| view.warm_history)
            .unwrap_or(false)
    }

    /// How many first-parent trunk commits a red judgement reads back (`[red] window`, NEW-6).
    #[must_use]
    pub fn red_window(&self) -> usize {
        self.red
            .as_ref()
            .and_then(|red| red.window)
            .unwrap_or(crate::red::DEFAULT_WINDOW)
    }

    /// The instruction files (`[agent] instructions`, G155), paths relative to the top level.
    #[must_use]
    pub fn instructions(&self) -> Vec<String> {
        self.agent
            .as_ref()
            .and_then(|agent| agent.instructions.clone())
            .unwrap_or_else(|| DEFAULT_INSTRUCTIONS.map(str::to_owned).to_vec())
    }

    #[must_use]
    pub fn trunk(&self) -> &str {
        self.trunk.as_deref().unwrap_or("main")
    }

    #[must_use]
    pub fn remote(&self) -> &str {
        self.remote.as_deref().unwrap_or("origin")
    }

    #[must_use]
    pub fn intent_trailer(&self) -> &str {
        self.trailers
            .as_ref()
            .and_then(|t| t.intent.as_deref())
            .unwrap_or("Intent")
    }

    #[must_use]
    pub fn actor_trailer(&self) -> &str {
        self.trailers
            .as_ref()
            .and_then(|t| t.actor.as_deref())
            .unwrap_or("Actor")
    }

    /// The retention window; one week unless configured.
    #[must_use]
    pub fn retention_window(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.retention
                .as_ref()
                .and_then(|retention| retention.window_s)
                .unwrap_or(7 * 24 * 3600),
        )
    }

    /// The ref-growth budget (§ 10a): bay refs on the remote past which markers are compacted.
    #[must_use]
    pub fn ref_budget(&self) -> u64 {
        self.retention
            .as_ref()
            .and_then(|retention| retention.ref_budget)
            .unwrap_or(crate::compact::DEFAULT_BUDGET)
    }

    #[must_use]
    pub fn lease(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.executor
                .as_ref()
                .and_then(|executor| executor.lease_s)
                .unwrap_or(60),
        )
    }

    /// A queue head's progress deadline (§ 6): the execution budget (default 30 minutes) plus
    /// the publish window (default 60 s).
    #[must_use]
    pub fn progress_deadline(&self) -> std::time::Duration {
        let window = std::time::Duration::from_secs(
            self.executor
                .as_ref()
                .and_then(|executor| executor.publish_window_s)
                .unwrap_or(60),
        );
        self.execution_budget()
            .unwrap_or(std::time::Duration::from_mins(30))
            + window
    }

    #[must_use]
    pub fn takeover(&self) -> std::time::Duration {
        self.executor
            .as_ref()
            .and_then(|executor| executor.takeover_s)
            .map_or_else(|| self.lease() * 2, std::time::Duration::from_secs)
    }

    #[must_use]
    pub fn heartbeat(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.executor
                .as_ref()
                .and_then(|executor| executor.heartbeat_s)
                .unwrap_or(10),
        )
    }

    #[must_use]
    pub fn submission_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.executor
                .as_ref()
                .and_then(|executor| executor.submission_timeout_s)
                .unwrap_or(30),
        )
    }

    /// Whether the executor finds the culprit of a red post check (`[runner] bisect_post`).
    #[must_use]
    pub fn bisect_post(&self) -> bool {
        self.executor
            .as_ref()
            .and_then(|executor| executor.bisect_post)
            .unwrap_or(false)
    }

    #[must_use]
    pub fn execution_budget(&self) -> Option<std::time::Duration> {
        self.executor
            .as_ref()
            .and_then(|executor| executor.execution_budget_s)
            .map(std::time::Duration::from_secs)
    }

    /// `[runner] parallel_checks` (N-17, `crate::check::run_all`): how many of a candidate's
    /// checks run at once. Default 1, one after another.
    #[must_use]
    pub fn parallel_checks(&self) -> usize {
        self.executor
            .as_ref()
            .and_then(|executor| executor.parallel_checks)
            .unwrap_or(1)
    }

    /// `[runner] ahead` (NEW-2, `crate::throughput`): how many entries behind the queue's
    /// head are checked ahead of their turn. Default 0, off.
    #[must_use]
    pub fn ahead(&self) -> usize {
        self.executor
            .as_ref()
            .and_then(|executor| executor.ahead)
            .unwrap_or(0)
    }

    /// `[runner] footprint_lanes` (NEW-10, `crate::throughput`). Default off.
    #[must_use]
    pub fn footprint_lanes(&self) -> bool {
        self.executor
            .as_ref()
            .and_then(|executor| executor.footprint_lanes)
            .unwrap_or(false)
    }

    /// `[runner] batch` (`crate::throughput`), with its defaults; `None` when batching is off.
    #[must_use]
    pub fn batch(&self) -> Option<crate::throughput::Bounds> {
        let batch = self.executor.as_ref()?.batch?;
        Some(crate::throughput::Bounds {
            min: batch.min.unwrap_or(1),
            max: batch.max.unwrap_or(crate::throughput::DEFAULT_BATCH_MAX),
            max_failure_rate: batch
                .max_failure_rate
                .unwrap_or(crate::throughput::DEFAULT_MAX_FAILURE_RATE),
        })
    }

    /// The Claude adapter is configured by `[agent] product = "claude"` (`DESIGN` § 15.2).
    #[must_use]
    pub fn claude_adapter(&self) -> bool {
        self.agent
            .as_ref()
            .is_some_and(|agent| agent.product == CLAUDE_PRODUCT)
    }

    /// The Codex adapter is configured by `[agent] product = "codex"` (G308). Its hooks only
    /// sense: Codex runs a tool whose hook fails, so the guard stays at trunk.
    #[must_use]
    pub fn codex_adapter(&self) -> bool {
        self.agent
            .as_ref()
            .is_some_and(|agent| agent.product == CODEX_PRODUCT)
    }

    /// The retired keys this config carries, which bay accepts and never reads, as doctor
    /// names them: `[check] command` (ruling Q14) and the `[update]` keys.
    #[must_use]
    pub fn ignored(&self) -> Vec<String> {
        [("check", &self.check), ("update", &self.update)]
            .into_iter()
            .filter_map(|(table, keys)| Some((table, keys.as_ref()?)))
            .flat_map(|(table, keys)| {
                let named: Vec<String> =
                    keys.keys().map(|key| format!("[{table}] {key}")).collect();
                if named.is_empty() {
                    vec![format!("[{table}]")]
                } else {
                    named
                }
            })
            .collect()
    }
}

/// Whether `name` can be a target's ref: a full ref name under `refs/`, outside bay's own
/// namespaces, in the characters Git takes in every ref, with no empty or dot-led component.
fn is_target_ref(name: &str) -> bool {
    name.starts_with("refs/")
        && !name.starts_with(crate::names::BAY)
        && !name.starts_with(crate::names::NOTES)
        && !std::path::Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("lock"))
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
        && name
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.'))
        && !name.contains("..")
}

#[cfg(test)]
mod tests {
    use super::Config;

    /// `INTERFACES` § 7's example `.bay/config.toml`, verbatim.
    const INTERFACES_7: &str = r#"
main = "main"
remote = "origin"
[trailers]            # map Intent and Actor to an organisation's own keys (defaults shown);
                      # Change-Id, Revision-Id, Request-Id, Candidate-Id, Registry-Id and
                      # Bay-Epoch are fixed names
intent = "Intent"
actor = "Actor"
[plugin.notify]
command = "scripts/bay-notify"
[policy]
profile = "default"                # completion dimensions per change kind
evidence_reuse = "off"             # "exact-inputs" once its test (G7) passes
[hooks]
cache_max_age_s = 30               # older coordination cache → writes to declared inputs get COVERAGE_UNKNOWN
[agent]
product = "claude"                 # leave this table out when the box does not use an agent adapter. Another product names its own adapter (`DESIGN` § 15)
"#;

    #[test]
    fn every_key_interfaces_7_lists_is_read() {
        let config: Config = toml::from_str(INTERFACES_7).unwrap();
        assert_eq!(config.trunk(), "main");
        assert_eq!(config.remote(), "origin");
        assert_eq!(config.intent_trailer(), "Intent");
        assert_eq!(config.actor_trailer(), "Actor");
        assert!(config.claude_adapter());
        let notify = config.plugin.notify.as_ref().unwrap();
        assert_eq!(notify.command.as_deref(), Some("scripts/bay-notify"));
        let policy = config.policy.as_ref().unwrap();
        assert_eq!(policy.profile.as_deref(), Some("default"));
        assert_eq!(policy.evidence_reuse.as_deref(), Some("off"));
        let hooks = config.hooks.as_ref().unwrap();
        assert_eq!(hooks.cache_max_age_s, Some(30));
    }

    #[test]
    fn a_listed_key_of_the_wrong_type_does_not_load() {
        // A key bay reads is typed; a mistyped one is a config error, never a silent default.
        assert!(toml::from_str::<Config>("[hooks]\ncache_max_age_s = \"30\"\n").is_err());
        assert!(toml::from_str::<Config>("[plugin.notify]\ncommand = 1\n").is_err());
        assert!(Config::parse("[hooks]\nnotice_budget_bytes = -1\n").is_err());
    }

    #[test]
    fn the_notice_budget_is_2048_bytes_unless_configured() {
        // Ruling R6.
        assert_eq!(Config::default().notice_budget(), 2048);
        let config = Config::parse("[hooks]\nnotice_budget_bytes = 512\n").unwrap();
        assert_eq!(config.notice_budget(), 512);
    }

    #[test]
    fn the_idle_checkpoint_is_600_seconds_unless_configured() {
        // G165.
        assert_eq!(Config::default().checkpoint_idle().as_secs(), 600);
        let config = Config::parse("[hooks]\ncheckpoint_idle_s = 5\n").unwrap();
        assert_eq!(config.checkpoint_idle().as_secs(), 5);
    }

    #[test]
    fn the_history_warm_up_is_off_unless_configured() {
        // G175.
        assert!(!Config::default().warm_history());
        assert!(
            Config::parse("[view]\nwarm_history = true\n")
                .unwrap()
                .warm_history()
        );
        assert!(
            !Config::parse("[view]\nwarm_history = false\n")
                .unwrap()
                .warm_history()
        );
        assert!(Config::parse("[view]\nwarm_history = \"yes\"\n").is_err());
        assert!(Config::parse("[view]\nwarm = true\n").is_err());
    }

    #[test]
    fn the_red_window_is_50_commits_unless_configured() {
        // NEW-6.
        assert_eq!(Config::default().red_window(), 50);
        assert_eq!(
            Config::parse("[red]\nwindow = 3\n").unwrap().red_window(),
            3
        );
        for text in [
            "[red]\nwindow = 0\n",
            "[red]\nwindow = 10001\n",
            "[red]\nwindow = -1\n",
            "[red]\nwindow = \"50\"\n",
            "[red]\nwidth = 50\n",
        ] {
            assert!(Config::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_key_bay_does_not_know_does_not_load() {
        // A misspelled key is an error that names it, never a silent default.
        let error = Config::parse("[hooks]\ncache_max_age = 30\n").unwrap_err();
        assert!(error.contains("cache_max_age"), "{error}");
        assert!(Config::parse("trnk = \"main\"\n").is_err());
        assert!(Config::parse("[hook]\ncache_max_age_s = 30\n").is_err());
        assert!(Config::parse("[runner]\nlease = 5\n").is_err());
    }

    #[test]
    fn a_project_floor_is_no_key() {
        // Owner ruling (BACKLOG 54): no project floor; `bay = "<version>"` is a key bay does not
        // know, so it is refused like any other.
        let error = Config::parse("bay = \"0.0.1\"\n").unwrap_err();
        assert!(error.contains("bay"), "{error}");
    }

    #[test]
    fn a_retired_key_loads_and_is_named_as_ignored() {
        // Ruling Q14: an older config's `[check] command` loads, and is read by nothing.
        assert_eq!(Config::parse("").unwrap().ignored(), Vec::<String>::new());
        let config = Config::parse("[check]\ncommand = \"scripts/bay-check\"\n").unwrap();
        assert_eq!(config.ignored(), ["[check] command"]);
        let config = Config::parse("[check]\n[update]\nauto = false\n").unwrap();
        assert_eq!(config.ignored(), ["[check]", "[update] auto"]);
    }

    #[test]
    fn evidence_reuse_is_off_or_exact_inputs() {
        assert!(!Config::parse("").unwrap().evidence_reuse());
        assert!(
            !Config::parse("[policy]\nevidence_reuse = \"off\"\n")
                .unwrap()
                .evidence_reuse()
        );
        assert!(
            Config::parse("[policy]\nevidence_reuse = \"exact-inputs\"\n")
                .unwrap()
                .evidence_reuse()
        );
        // A misspelled value never loads as a silent default, on or off.
        let error = Config::parse("[policy]\nevidence_reuse = \"exact\"\n").unwrap_err();
        assert!(error.contains("\"exact\""), "{error}");
    }

    #[test]
    fn the_takeover_window_is_longer_than_the_lease() {
        assert!(Config::parse("[runner]\nlease_s = 5\ntakeover_s = 6\n").is_ok());
        assert!(Config::parse("[runner]\nlease_s = 5\n").is_ok());
        for text in [
            "[runner]\nlease_s = 5\ntakeover_s = 5\n",
            "[runner]\ntakeover_s = 0\n",
            "[runner]\ntakeover_s = 30\n",
        ] {
            let error = Config::parse(text).unwrap_err();
            assert!(error.contains("takeover_s"), "{text}: {error}");
        }
    }

    #[test]
    fn a_repository_never_chooses_no_sandbox() {
        assert!(Config::parse("[runner]\nlease_s = 5\n").is_ok());
        for value in ["\"none\"", "\"bwrap\"", "\"\"", "true"] {
            let refused = Config::parse(&format!("[runner]\nsandbox = {value}\n")).unwrap_err();
            assert!(refused.contains("bay.runnerSandbox"), "{value}: {refused}");
        }
    }

    #[test]
    fn targets_and_freezes_are_read_and_checked() {
        // NEW-5, NEW-9.
        let config = Config::parse(
            "[[target]]\nname = \"staging\"\nrequires = [\"suite\"]\n\
             [[target]]\nname = \"production\"\nfrom = \"staging\"\nsoak_s = 3600\n\
             ref = \"refs/heads/prod\"\n\
             [[hold]]\ntarget = \"production\"\nend = \"2026-12-24T00:00:00Z\"\n\
             reason = \"holiday\"\n",
        )
        .unwrap();
        let staging = config.target("staging").unwrap();
        assert_eq!(staging.reference(), "refs/heads/staging");
        assert_eq!(staging.from_target("main"), None);
        assert_eq!(staging.requires, ["suite"]);
        let production = config.target("production").unwrap();
        assert_eq!(production.reference(), "refs/heads/prod");
        assert_eq!(production.from_target("main"), Some("staging"));
        assert_eq!(production.soak(), 3600);
        assert!(!staging.advance, "a target advances only when it says so");
        let advancing = Config::parse(
            "[[target]]\nname = \"staging\"\nrequires = [\"suite\"]\nadvance = true\n",
        )
        .unwrap();
        assert!(advancing.target("staging").unwrap().advance);
        let end = crate::clock::unix_of("2026-12-24T00:00:00Z").unwrap();
        assert!(config.freezes[0].active(end - 1));
        assert!(!config.freezes[0].active(end));
        assert!(
            Config::parse("[[target]]\nname = \"staging\"\nfrom = \"main\"\n").is_ok(),
            "the trunk by name"
        );
        for (text, named) in [
            ("[[target]]\nname = \"Staging\"\n", "Staging"),
            ("[[target]]\nname = \"main\"\n", "main"),
            (
                "[[target]]\nname = \"staging\"\nref = \"refs/heads/main\"\n",
                "ref",
            ),
            (
                "[[target]]\nname = \"staging\"\nref = \"refs/bay/x\"\n",
                "ref",
            ),
            (
                "[[target]]\nname = \"staging\"\nref = \"heads/staging\"\n",
                "ref",
            ),
            (
                "[[target]]\nname = \"staging\"\n[[target]]\nname = \"staging\"\n",
                "two",
            ),
            ("[[target]]\nname = \"staging\"\nfrom = \"dev\"\n", "dev"),
            (
                "[[target]]\nname = \"a\"\nfrom = \"b\"\n[[target]]\nname = \"b\"\nfrom = \"a\"\n",
                "cycle",
            ),
            ("[[target]]\nname = \"staging\"\nsoak = 5\n", "soak"),
            (
                "[[target]]\nname = \"staging\"\nadvance = true\n",
                "advance = true needs requires",
            ),
            (
                "[[hold]]\ntarget = \"staging\"\nend = \"2026-12-24T00:00:00Z\"\nreason = \"r\"\n",
                "names neither",
            ),
            (
                "[[target]]\nname = \"staging\"\n[[hold]]\ntarget = \"staging\"\nend = \"2026-12-24\"\nreason = \"r\"\n",
                "end",
            ),
            (
                "[[target]]\nname = \"staging\"\n[[hold]]\ntarget = \"staging\"\nend = \"2026-12-24T00:00:00Z\"\nreason = \" \"\n",
                "reason",
            ),
        ] {
            let error = Config::parse(text).unwrap_err();
            assert!(error.contains(named), "{text}: {error}");
        }
    }

    #[test]
    fn a_freeze_of_the_trunk_and_the_hold_adapter_are_read_and_checked() {
        // N-8: a freeze may name the trunk itself, with no [[target]].
        let config = Config::parse(
            "main = \"main\"\n[[hold]]\ntarget = \"main\"\nend = \"2026-12-24T00:00:00Z\"\n\
             reason = \"migration\"\n[plugin.hold]\ncommand = \"scripts/bay-hold\"\ntimeout_s = 5\n",
        )
        .unwrap();
        assert_eq!(config.freezes[0].target, "main");
        // 0.3.0: a `[[hold]]` with no target holds main.
        let untargeted = Config::parse(
            "main = \"trunk\"\n[[hold]]\nend = \"2026-12-24T00:00:00Z\"\nreason = \"migration\"\n",
        )
        .unwrap();
        assert_eq!(untargeted.freezes[0].target, "trunk");
        let hold = config.plugin.hold.as_ref().unwrap();
        assert_eq!(hold.command.as_deref(), Some("scripts/bay-hold"));
        assert_eq!(hold.timeout_s, Some(5));
        for (text, named) in [
            (
                "[[hold]]\ntarget = \"main\"\nend = \"2026-12-24\"\nreason = \"r\"\n",
                "end",
            ),
            (
                "[[hold]]\ntarget = \"main\"\nend = \"2026-12-24T00:00:00Z\"\nreason = \"\"\n",
                "reason",
            ),
            ("[plugin.hold]\ncommand = \" \"\n", "empty"),
            (
                "[plugin.hold]\ncommand = \"x\"\ntimeout_s = 0\n",
                "timeout_s",
            ),
            ("[plugin.hold]\ncommand = \"x\"\nevery = 1\n", "every"),
        ] {
            let error = Config::parse(text).unwrap_err();
            assert!(error.contains(named), "{text}: {error}");
        }
    }

    #[test]
    fn the_prune_adapter_is_read_and_checked() {
        // N-19.
        assert!(Config::default().plugin.prune.is_none());
        let config =
            Config::parse("[plugin.prune]\ncommand = \"scripts/on-prune\"\ntimeout_s = 5\n")
                .unwrap();
        let heads = config.plugin.prune.as_ref().unwrap();
        assert_eq!(heads.command.as_deref(), Some("scripts/on-prune"));
        assert_eq!(heads.timeout_s, Some(5));
        for (text, named) in [
            ("[plugin.prune]\ncommand = \" \"\n", "empty"),
            (
                "[plugin.prune]\ncommand = \"x\"\ntimeout_s = 0\n",
                "timeout_s",
            ),
            (
                "[plugin.prune]\ncommand = \"x\"\non_delete = \"y\"\n",
                "on_delete",
            ),
        ] {
            let error = Config::parse(text).unwrap_err();
            assert!(error.contains(named), "{text}: {error}");
        }
    }

    #[test]
    fn a_lane_is_named_once_with_ref_patterns_and_a_shorter_window() {
        // NEW-4.
        assert!(Config::default().lanes.is_empty());
        let config = Config::parse(
            "[[lane]]\nname = \"research\"\nrefs = [\"refs/heads/research/*\"]\n\
             [[lane]]\nname = \"prototype\"\nrefs = [\"refs/heads/proto-*\", \"refs/sandbox/x\"]\nwindow_s = 60\n",
        )
        .unwrap();
        assert_eq!(config.lanes.len(), 2);
        let research = config.lane("research").unwrap();
        assert_eq!(config.lane_window(research).as_secs(), 24 * 3600);
        let prototype = config.lane("prototype").unwrap();
        assert_eq!(config.lane_window(prototype).as_secs(), 60);
        assert!(config.lane("other").is_none());
        // Under a retention window shorter than a day, an unnamed lane window is that window.
        let short = Config::parse(
            "[retention]\nwindow_s = 600\n[[lane]]\nname = \"research\"\nrefs = [\"refs/heads/research/*\"]\n",
        )
        .unwrap();
        assert_eq!(short.lane_window(&short.lanes[0]).as_secs(), 600);
        for (text, named) in [
            (
                "[[lane]]\nname = \"Research\"\nrefs = [\"refs/heads/r/*\"]\n",
                "name",
            ),
            (
                "[[lane]]\nname = \"\"\nrefs = [\"refs/heads/r/*\"]\n",
                "name",
            ),
            (
                "[[lane]]\nname = \"r\"\nrefs = [\"refs/heads/a/*\"]\n[[lane]]\nname = \"r\"\nrefs = [\"refs/heads/b/*\"]\n",
                "twice",
            ),
            ("[[lane]]\nname = \"r\"\nrefs = []\n", "refs"),
            (
                "[[lane]]\nname = \"r\"\nrefs = [\"research/*\"]\n",
                "research/*",
            ),
            (
                "[[lane]]\nname = \"r\"\nrefs = [\"refs/heads/*/*\"]\n",
                "at most one",
            ),
            (
                "[[lane]]\nname = \"r\"\nrefs = [\"refs/bay/changes/*\"]\n",
                "refs/bay/",
            ),
            (
                "[[lane]]\nname = \"r\"\nrefs = [\"refs/heads/*\"]\n",
                "main",
            ),
            (
                "[[lane]]\nname = \"r\"\nrefs = [\"refs/heads/main\"]\n",
                "main",
            ),
            (
                "[[lane]]\nname = \"r\"\nrefs = [\"refs/heads/r/*\"]\nwindow_s = 604800\n",
                "shorter",
            ),
            (
                "[[lane]]\nname = \"r\"\nrefs = [\"refs/heads/r/*\"]\nwindow = 5\n",
                "window",
            ),
            ("[[lane]]\nrefs = [\"refs/heads/r/*\"]\n", "name"),
        ] {
            let error = Config::parse(text).unwrap_err();
            assert!(error.contains(named), "{text}: {error}");
        }
    }

    #[test]
    fn the_executor_and_retention_keys_built_features_use_are_read() {
        let config: Config = toml::from_str(
            "[runner]\nlease_s = 5\nheartbeat_s = 2\n[retention]\nwindow_s = 0\nref_budget = 7\n",
        )
        .unwrap();
        assert_eq!(config.lease().as_secs(), 5);
        assert_eq!(config.heartbeat().as_secs(), 2);
        assert_eq!(config.retention_window().as_secs(), 0);
        assert_eq!(config.ref_budget(), 7);
        assert_eq!(Config::default().ref_budget(), 100_000);
    }

    /// N-17: a candidate's checks run one after another by default, and at most
    /// `MAX_PARALLEL_CHECKS` at once.
    #[test]
    fn parallel_checks_default_to_one_and_are_bounded() {
        assert_eq!(Config::default().parallel_checks(), 1);
        let on = Config::parse("[runner]\nparallel_checks = 3\n").unwrap();
        assert_eq!(on.parallel_checks(), 3);
        for bad in [0, crate::check::MAX_PARALLEL_CHECKS + 1] {
            let error = Config::parse(&format!("[runner]\nparallel_checks = {bad}\n")).unwrap_err();
            assert!(error.contains("parallel_checks"), "{error}");
        }
        assert!(Config::parse("[runner]\nparallel_checks = 16\n").is_ok());
    }

    /// `crate::throughput`: every lever is off by default, in its bounds, and turns exact-inputs
    /// reuse on (0.3.0), since a check made ahead of its turn counts only through reuse; a lever
    /// with reuse turned off is refused.
    #[test]
    fn the_throughput_levers_are_off_bounded_and_imply_reuse() {
        const OFF: &str = "[policy]\nevidence_reuse = \"off\"\n";
        assert_eq!(Config::default().ahead(), 0);
        assert!(!Config::default().evidence_reuse());
        let on = Config::parse("[runner]\nahead = 2\n").unwrap();
        assert_eq!(on.ahead(), 2);
        assert!(on.evidence_reuse());
        let error = Config::parse(&format!("{OFF}[runner]\nahead = 2\n")).unwrap_err();
        assert!(error.contains("evidence_reuse"), "{error}");
        let error = Config::parse("[runner]\nahead = 17\n").unwrap_err();
        assert!(error.contains("at most 16"), "{error}");
        assert!(
            !Config::parse("[runner]\nahead = 0\n")
                .unwrap()
                .evidence_reuse()
        );
        assert!(!Config::default().footprint_lanes());
        let lanes = Config::parse("[runner]\nfootprint_lanes = true\n").unwrap();
        assert!(lanes.footprint_lanes() && lanes.evidence_reuse());
        let error = Config::parse(&format!("{OFF}[runner]\nfootprint_lanes = true\n")).unwrap_err();
        assert!(error.contains("footprint_lanes"), "{error}");
        assert_eq!(Config::default().batch(), None);
        let batch = Config::parse("[runner]\nbatch = { max = 4 }\n")
            .unwrap()
            .batch()
            .unwrap();
        assert_eq!((batch.min, batch.max), (1, 4));
        assert!((batch.max_failure_rate - 0.3).abs() < f64::EPSILON);
        for bad in [
            "batch = { min = 0 }",
            "batch = { min = 5, max = 4 }",
            "batch = { max = 65 }",
            "batch = { max_failure_rate = 1.5 }",
            "batch = { size = 2 }",
        ] {
            assert!(
                Config::parse(&format!("[runner]\n{bad}\n")).is_err(),
                "{bad}"
            );
        }
        assert!(Config::parse(&format!("{OFF}[runner]\nbatch = {{ max = 4 }}\n")).is_err());
    }
}

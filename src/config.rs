//! `.bay/config.toml` in the repository (`INTERFACES` § 7).
//!
//! Every key § 7 lists is parsed and typed, so a mistyped one is a config error. Retired keys
//! (`[check] command`, `[update]`) are accepted and ignored, and doctor names them
//! ([`Config::ignored`]). `[runtime] command` (the runtime adapter, `INTERFACES` § 4) is
//! not in § 7's list yet. `[executor]` and `[retention]` are read by built features but are
//! not in § 7's list yet (BACKLOG 18), nor is `[view]` (G175). A key or table bay does not know is a config error that
//! names it, as is `evidence_reuse` set to anything but `"off"` or `"exact-inputs"`: a misspelled
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
    pub trunk: Option<String>,
    pub remote: Option<String>,
    pub agent: Option<Agent>,
    pub trailers: Option<Trailers>,
    pub executor: Option<Executor>,
    pub retention: Option<Retention>,
    /// `[check]` is retired (ruling Q14): checks are declared in `.bay/policy.toml`, whose
    /// `[[check]]` entries name their own commands, so the command a check runs has one
    /// definition, inside the policy's revision and digest (G141). An older config that still
    /// carries the table keeps loading; nothing reads it but [`Config::ignored`].
    check: Option<toml::Table>,
    pub notify: Option<Notify>,
    /// `[wake] command`: the wake adapter (G58), asked once when this machine accepts a talk.
    pub wake: Option<Wake>,
    pub runtime: Option<Runtime>,
    pub policy: Option<PolicyProfile>,
    pub hooks: Option<Hooks>,
    pub view: Option<View>,
    /// `[update]` is accepted and ignored: the update pin and switch are the box's
    /// (`git config --global bay.update*`, `DESIGN` § 15.6), never a repository's, and an older
    /// config that still carries the table must keep loading.
    update: Option<toml::Table>,
}

/// `[notify] command`: the notify adapter (`INTERFACES` § 4), called once after each event is
/// written (`crate::notify`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notify {
    pub command: Option<String>,
}

/// `[wake] command`: the wake adapter (G58, `crate::talk::wake`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wake {
    pub command: Option<String>,
}

/// `[runtime] command`: the runtime adapter (`INTERFACES` § 4), asked by `bay status` for
/// `running` and `effective` records about a landed commit (`crate::runtime`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    pub command: Option<String>,
}

/// `[policy]`: the completion profile and evidence reuse ([`Config::evidence_reuse`]).
/// `profile` names the `[profile.<name>]` of `.bay/policy.toml` that `bay status` judges
/// completion by (`crate::completion`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyProfile {
    pub profile: Option<String>,
    pub evidence_reuse: Option<String>,
}

/// `[hooks] cache_max_age_s` (`DESIGN` § 11): the coordination cache's maximum age for
/// `pre-tool` (`crate::coord`). Default 30 (`INTERFACES` § 7). `[hooks] notice_budget_bytes`
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

/// How long a closed change keeps its drafts before the retention cut (§ 5), and the bay ref
/// count past which an executor compacts markers (§ 10a ref-growth budget, `crate::compact`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    pub window_s: Option<u64>,
    pub ref_budget: Option<u64>,
}

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
    /// Never a repository's setting (ruling R14): whether checks run with no sandbox is the
    /// box's choice, global Git config `bay.executorSandbox = none`. Read only to refuse it.
    pub sandbox: Option<toml::Value>,
}

/// The configurable trailer keys. `Change-Id`, `Revision-Id`, `Request-Id`, `Candidate-Id`,
/// `Registry-Id`, `Bay-Epoch` and `Bay-Session` are fixed names. The actor trailer carries
/// the actor's bare name; its session goes in `Bay-Session`.
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
        let config: Self = toml::from_str(text).map_err(|error| error.to_string())?;
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
                "[executor] sandbox is not a repository setting (ruling R14): running \
                 checks with no sandbox is the box's choice, git config --global \
                 bay.executorSandbox none"
                    .to_owned(),
            );
        }
        // § 4: the takeover window is longer than the executor lease.
        if config.takeover() <= config.lease() {
            return Err(format!(
                "[executor] takeover_s = {}: the takeover window must be longer than the lease \
                 (lease_s = {})",
                config.takeover().as_secs(),
                config.lease().as_secs()
            ));
        }
        Ok(config)
    }

    /// Whether `[policy] evidence_reuse = "exact-inputs"`: a producer-signed record whose tree,
    /// base tree, commit (for a commit-keyed check), check, environment, manifest, revision,
    /// provenance and fixtures all match stands for a new run (`DESIGN` § 7, G7). Off by
    /// default; without a producer registry, or for a change with named inputs, nothing is
    /// reused.
    #[must_use]
    pub fn evidence_reuse(&self) -> bool {
        self.policy
            .as_ref()
            .and_then(|policy| policy.evidence_reuse.as_deref())
            == Some("exact-inputs")
    }

    /// The coordination cache's maximum age for `pre-tool`: older, a write to a declared input
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

    #[must_use]
    pub fn execution_budget(&self) -> Option<std::time::Duration> {
        self.executor
            .as_ref()
            .and_then(|executor| executor.execution_budget_s)
            .map(std::time::Duration::from_secs)
    }

    /// The Claude adapter is configured by `[agent] product = "claude"` (`DESIGN` § 15.2).
    #[must_use]
    pub fn claude_adapter(&self) -> bool {
        self.agent
            .as_ref()
            .is_some_and(|agent| agent.product == "claude")
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

#[cfg(test)]
mod tests {
    use super::Config;

    /// `INTERFACES` § 7's example `.bay/config.toml`, verbatim.
    const INTERFACES_7: &str = r#"
trunk = "main"
remote = "origin"
[trailers]            # map Intent and Actor to an organisation's own keys (defaults shown);
                      # Change-Id, Revision-Id, Request-Id, Candidate-Id, Registry-Id and
                      # Bay-Epoch are fixed names
intent = "Intent"
actor = "Actor"
[notify]
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
        let notify = config.notify.as_ref().unwrap();
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
        assert!(toml::from_str::<Config>("[notify]\ncommand = 1\n").is_err());
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
    fn a_key_bay_does_not_know_does_not_load() {
        // A misspelled key is an error that names it, never a silent default.
        let error = Config::parse("[hooks]\ncache_max_age = 30\n").unwrap_err();
        assert!(error.contains("cache_max_age"), "{error}");
        assert!(Config::parse("trnk = \"main\"\n").is_err());
        assert!(Config::parse("[hook]\ncache_max_age_s = 30\n").is_err());
        assert!(Config::parse("[executor]\nlease = 5\n").is_err());
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
        assert!(Config::parse("[executor]\nlease_s = 5\ntakeover_s = 6\n").is_ok());
        assert!(Config::parse("[executor]\nlease_s = 5\n").is_ok());
        for text in [
            "[executor]\nlease_s = 5\ntakeover_s = 5\n",
            "[executor]\ntakeover_s = 0\n",
            "[executor]\ntakeover_s = 30\n",
        ] {
            let error = Config::parse(text).unwrap_err();
            assert!(error.contains("takeover_s"), "{text}: {error}");
        }
    }

    #[test]
    fn a_repository_never_chooses_no_sandbox() {
        assert!(Config::parse("[executor]\nlease_s = 5\n").is_ok());
        for value in ["\"none\"", "\"bwrap\"", "\"\"", "true"] {
            let refused = Config::parse(&format!("[executor]\nsandbox = {value}\n")).unwrap_err();
            assert!(
                refused.contains("bay.executorSandbox"),
                "{value}: {refused}"
            );
        }
    }

    #[test]
    fn the_executor_and_retention_keys_built_features_use_are_read() {
        let config: Config = toml::from_str(
            "[executor]\nlease_s = 5\nheartbeat_s = 2\n[retention]\nwindow_s = 0\nref_budget = 7\n",
        )
        .unwrap();
        assert_eq!(config.lease().as_secs(), 5);
        assert_eq!(config.heartbeat().as_secs(), 2);
        assert_eq!(config.retention_window().as_secs(), 0);
        assert_eq!(config.ref_budget(), 7);
        assert_eq!(Config::default().ref_budget(), 100_000);
    }
}

//! `.bay/config.toml` in the repository (`INTERFACES` § 7).
//!
//! Every key § 7 lists is parsed and typed, so a mistyped one is a config error. Keys whose
//! feature is not built yet (`[check] command`, `[notify] command`, `[policy]`, `[hooks]`) are
//! held and read by nothing. `[executor]` and `[retention]` are read by built features but are
//! not in § 7's list yet (BACKLOG 18). A key or table bay does not know is a config error that
//! names it, as is `evidence_reuse` set to anything but `"off"`: a misspelled or unbuilt
//! setting never loads as a silent default. A config written for a later bay is refused by
//! this one, and the recovery is `bay update`.

use std::path::Path;

use serde::Deserialize;

pub const CONFIG_PATH: &str = ".bay/config.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub trunk: Option<String>,
    pub remote: Option<String>,
    pub agent: Option<Agent>,
    pub trailers: Option<Trailers>,
    pub executor: Option<Executor>,
    pub retention: Option<Retention>,
    pub check: Option<Check>,
    pub notify: Option<Notify>,
    pub policy: Option<PolicyProfile>,
    pub hooks: Option<Hooks>,
    /// `[update]` is accepted and ignored: the update pin, channel and switch are the box's
    /// (`git config --global bay.update*`, `DESIGN` § 15.6), never a repository's, and an older
    /// config that still carries the table must keep loading.
    #[allow(dead_code)]
    update: Option<toml::Table>,
}

/// `[check] command`: the check adapter. Not read: checks are declared in `.bay/policy.toml`
/// (§ 7), whose `[[check]]` entries name their own commands.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub command: Option<String>,
}

/// `[notify] command`: the notify adapter (`INTERFACES` § 4). Not read until events are built.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notify {
    pub command: Option<String>,
}

/// `[policy]`: the completion profile and evidence reuse. Not read: reuse stays off until G7
/// passes, and one profile is built.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyProfile {
    pub profile: Option<String>,
    pub evidence_reuse: Option<String>,
}

/// `[hooks] cache_max_age_s` (`DESIGN` § 11): the coordination cache's maximum age for
/// `pre-tool`. Not read until `pre-tool` judges writes against that cache.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    pub cache_max_age_s: Option<u64>,
}

/// How long a closed change keeps its drafts before the retention cut (§ 5).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retention {
    pub window_s: Option<u64>,
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
}

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
    /// A config that exists but does not parse.
    pub fn load_or_committed(root: &Path, git: &crate::git::Git) -> Result<Self, String> {
        if root.join(CONFIG_PATH).exists() {
            return Self::load(root);
        }
        match git.run(&["cat-file", "blob", &format!("HEAD:{CONFIG_PATH}")]) {
            Ok(text) => Self::parse(&text).map_err(|error| format!("HEAD:{CONFIG_PATH}: {error}")),
            Err(_) => Ok(Self::default()),
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
            && reuse != "off"
        {
            return Err(format!(
                "[policy] evidence_reuse = \"{reuse}\": this bay builds only \"off\" (evidence \
                 reuse waits for G7)"
            ));
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
[check]
command = "scripts/bay-check"      # the check adapter
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
        let check = config.check.as_ref().unwrap();
        assert_eq!(check.command.as_deref(), Some("scripts/bay-check"));
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
    fn evidence_reuse_is_off_until_g7() {
        assert!(Config::parse("[policy]\nevidence_reuse = \"off\"\n").is_ok());
        let error = Config::parse("[policy]\nevidence_reuse = \"exact-inputs\"\n").unwrap_err();
        assert!(error.contains("exact-inputs"), "{error}");
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
    fn the_executor_and_retention_keys_built_features_use_are_read() {
        let config: Config =
            toml::from_str("[executor]\nlease_s = 5\nheartbeat_s = 2\n[retention]\nwindow_s = 0\n")
                .unwrap();
        assert_eq!(config.lease().as_secs(), 5);
        assert_eq!(config.heartbeat().as_secs(), 2);
        assert_eq!(config.retention_window().as_secs(), 0);
    }
}

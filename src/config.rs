//! `.bay/config.toml` in the repository (`INTERFACES` § 7).
//!
//! Only the keys a built feature reads are parsed. Unknown keys are ignored, so a
//! config written for a later increment still loads.

use std::path::Path;

use serde::Deserialize;

pub const CONFIG_PATH: &str = ".bay/config.toml";

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    pub trunk: Option<String>,
    pub remote: Option<String>,
    pub agent: Option<Agent>,
    pub trailers: Option<Trailers>,
    pub executor: Option<Executor>,
    pub retention: Option<Retention>,
}

/// How long a closed change keeps its drafts before the retention cut (§ 5).
#[derive(Debug, Deserialize)]
pub struct Retention {
    pub window_s: Option<u64>,
}

/// An external executor implementing the contract of `INTERFACES` § 3a.
#[derive(Debug, Deserialize)]
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
/// `Registry-Id` and `Bay-Epoch` are fixed names.
#[derive(Debug, Deserialize)]
pub struct Trailers {
    pub intent: Option<String>,
    pub actor: Option<String>,
}

#[derive(Debug, Deserialize)]
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
            Ok(text) => {
                toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
            }
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
            Ok(text) => {
                toml::from_str(&text).map_err(|error| format!("HEAD:{CONFIG_PATH}: {error}"))
            }
            Err(_) => Ok(Self::default()),
        }
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

//! Named inputs and invalidation (`DESIGN` § 3.1, § 8; `INTERFACES` § 2, § 4; rulings R3, R4).
//!
//! A change declares a named input with `bay start --input <kind>:<name>`, beside its paths;
//! both are its `declared_inputs`. The acceptance policy names each input's provider in an
//! `[[input]]` entry, judged at the accepted base like a check adapter: providers are policy, so
//! a change can never supply its own. A provider is a small command that reads
//! `{"input": <name>, "tree": <sha>}` on stdin, runs in a materialization of that tree, and
//! prints `{"version": …, "consumers": [{"path": …, "kind": …}]}`.
//!
//! `observed_versions` (the version the actor read) is written by `start` and `start --resume`
//! at the change's base. `required_versions` are derived on read by running the provider at the
//! trunk tip, and never stored. `validated_candidate` is the attempt checked under them.
//!
//! Everything here reads local data only: Git objects and this repository's provider cache. A
//! provider runs in the check sandbox (`check::sandboxed`): the repository, its views and this
//! machine's keys are hidden from it. Its answer is cached under the digest of its `[[input]]`
//! entry, its adapter's object ids and the tree, and how far a caller trusts that cache is its
//! [`Probe`]: the land gate never reads it (a plain file anyone with the repository can write),
//! a view reads it before running a provider, and a hook reads only it and never runs one.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::canonical::digest;
use crate::change::{ChangeRecord, ChangeState};
use crate::git::Git;
use crate::policy;
use crate::repo::Repo;
use crate::result::Reason;

const DEFAULT_TIMEOUT_S: u64 = 30;

/// The longest `timeout_s` a policy may give a provider. A provider answers one question about
/// one tree, and every landing of a change that declares the input waits for it.
pub const MAX_TIMEOUT_S: u64 = 300;

/// A provider's scratch left by a run that was killed is swept once it is this old: twice the
/// longest a live run may take.
const SWEEP_AFTER: Duration = Duration::from_secs(2 * MAX_TIMEOUT_S);

/// How a caller gets a provider's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Runs the provider, never reading the cache: what the land gate and `start` record.
    Fresh,
    /// Reads the cache, and runs the provider on a miss: a view the user asked for.
    Run,
    /// Reads the cache only, and never runs a provider: a hook, which must stay cheap.
    Cached,
}

/// A named input: `<kind>:<name>`, for example `contract:api.orders`. The kind is lower-case
/// `[a-z][a-z0-9-]*`; the name is non-empty and holds no whitespace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InputName(String);

impl InputName {
    /// # Errors
    ///
    /// `text` is not `<kind>:<name>`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = || format!("{text:?} is not a named input <kind>:<name>, as in contract:api");
        let (kind, name) = text.split_once(':').ok_or_else(bad)?;
        let kind_ok = kind.starts_with(|c: char| c.is_ascii_lowercase())
            && kind
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !kind_ok || name.is_empty() || name.chars().any(char::is_whitespace) {
            return Err(bad());
        }
        Ok(Self(text.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for InputName {
    type Error = String;
    fn try_from(text: String) -> Result<Self, String> {
        Self::parse(&text)
    }
}

impl From<InputName> for String {
    fn from(name: InputName) -> Self {
        name.0
    }
}

impl std::fmt::Display for InputName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One entry of `declared_inputs` (`DESIGN` § 3.1): a path the view holds, or a named input.
/// A path is a plain string; a named input is `{"input": "<kind>:<name>"}`, so a path that
/// happens to hold a colon is never read as one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DeclaredInput {
    Path(String),
    Named { input: InputName },
}

/// The declared paths of `declared`, in order.
#[must_use]
pub fn paths(declared: &[DeclaredInput]) -> Vec<String> {
    declared
        .iter()
        .filter_map(|entry| match entry {
            DeclaredInput::Path(path) => Some(path.clone()),
            DeclaredInput::Named { .. } => None,
        })
        .collect()
}

/// The named inputs of `declared`, in order.
#[must_use]
pub fn names(declared: &[DeclaredInput]) -> Vec<InputName> {
    declared
        .iter()
        .filter_map(|entry| match entry {
            DeclaredInput::Named { input } => Some(input.clone()),
            DeclaredInput::Path(_) => None,
        })
        .collect()
}

/// A contract phase (`DESIGN` § 3.1, Parallel Change). While a contract is in `expand` or
/// `migrate` a change built on the old shape stays valid; only `contract` invalidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Expand,
    Migrate,
    Contract,
}

/// One `[[input]]` entry of `.bay/policy.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputEntry {
    pub name: InputName,
    /// The provider's command line; its executable is in `adapter`.
    pub provider: String,
    /// Every path the provider's answer is computed by, at the accepted base.
    pub adapter: Vec<String>,
    /// Absolute directories: the provider's `PATH`. With none, it runs with no `PATH`.
    #[serde(default)]
    pub path: Vec<String>,
    /// The contract phase; absent, every version change invalidates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
}

const fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_S
}

impl InputEntry {
    /// The validity rules of an entry at `commit` (`INTERFACES` § 7): the provider's
    /// executable is in its adapter list, every adapter path is present, `path` holds absolute
    /// directories, and `timeout_s` is between 1 and [`MAX_TIMEOUT_S`].
    ///
    /// # Errors
    ///
    /// The rule the entry breaks.
    pub fn validate(&self, git: &Git, commit: &str) -> Result<(), String> {
        let executable = self.provider.split_whitespace().next().unwrap_or_default();
        if !self
            .adapter
            .iter()
            .any(|path| policy::covers(path, executable))
        {
            return Err(format!(
                "input {} runs {executable}, which is not in its adapter list",
                self.name
            ));
        }
        if let Some(relative) = self
            .path
            .iter()
            .find(|dir| !Path::new(dir.as_str()).is_absolute() || dir.contains(':'))
        {
            return Err(format!(
                "input {} has the path entry {relative}; path takes absolute directories, each without ':'",
                self.name
            ));
        }
        // A timeout of zero would time every provider out: the input's coverage off, unsaid.
        if !(1..=MAX_TIMEOUT_S).contains(&self.timeout_s) {
            return Err(format!(
                "input {} has timeout_s = {}; it takes 1 to {MAX_TIMEOUT_S} seconds",
                self.name, self.timeout_s
            ));
        }
        adapter_ids(git, commit, &self.adapter).map(|_| ())
    }
}

/// The object id at `commit` of every adapter path.
fn adapter_ids(
    git: &Git,
    commit: &str,
    adapter: &[String],
) -> Result<BTreeMap<String, String>, String> {
    adapter
        .iter()
        .map(|path| {
            let spec = format!("{commit}:{}", path.trim_end_matches('/'));
            git.run(&["rev-parse", "--verify", "-q", &spec])
                .map(|oid| (path.clone(), oid))
                .map_err(|_| format!("adapter path {path} is absent"))
        })
        .collect()
}

/// How a consumer uses an input (`DESIGN` § 3.1). A historical citation never blocks work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerKind {
    Historical,
    Generated,
    Executable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consumer {
    pub path: String,
    pub kind: ConsumerKind,
}

/// A provider's answer for one tree (`INTERFACES` § 4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub version: String,
    #[serde(default)]
    pub consumers: Vec<Consumer>,
}

/// The attempt last checked under the required versions (`validated_candidate`, § 3.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Validated {
    /// The `Candidate-Id` of the attempt.
    pub candidate: String,
    /// The version of each named input at that attempt's base.
    pub versions: BTreeMap<String, String>,
}

/// The version of `entry`'s input in `commit`'s tree, by `entry`'s provider as it is at
/// `policy_commit` (the accepted base), got as `probe` says.
///
/// # Errors
///
/// The provider cannot be materialized, does not finish within its timeout, fails, or does not
/// print a version; or, under [`Probe::Cached`], the cache holds no answer: the input's
/// coverage is unknown.
pub fn version_at(
    repo: &Repo,
    policy_commit: &str,
    entry: &InputEntry,
    commit: &str,
    probe: Probe,
) -> Result<Version, String> {
    let git = &repo.git;
    let tree = git
        .run(&["rev-parse", &format!("{commit}^{{tree}}")])
        .map_err(|error| error.message())?;
    let ids = adapter_ids(git, policy_commit, &entry.adapter)?;
    let key = digest(&json!({"entry": entry, "adapter_ids": ids, "tree": tree}))?;
    let dir = repo.common_dir.join("bay").join("inputs");
    let cached = dir.join(format!("{}.json", key.trim_start_matches("sha256:")));
    if probe != Probe::Fresh
        && let Ok(text) = std::fs::read_to_string(&cached)
        && let Ok(version) = serde_json::from_str::<Version>(&text)
    {
        return Ok(version);
    }
    if probe == Probe::Cached {
        return Err(format!(
            "no answer of the provider for {} on tree {tree} in the local cache; a hook never runs a provider (bay status does)",
            entry.name
        ));
    }
    let runs = repo.common_dir.join("bay").join("inputs-run");
    sweep(&runs);
    let scratch = runs.join(format!(
        "run-{}-{}",
        std::process::id(),
        crate::ids::new_change_id().map_err(|error| error.to_string())?
    ));
    let answer = run_provider(repo, policy_commit, entry, commit, &tree, &scratch);
    let _ = std::fs::remove_dir_all(&scratch);
    let version = answer?;
    if let Ok(text) = serde_json::to_string(&version)
        && std::fs::create_dir_all(&dir).is_ok()
    {
        let partial = cached.with_extension("tmp");
        if std::fs::write(&partial, text).is_ok() {
            let _ = std::fs::rename(&partial, &cached);
        }
    }
    Ok(version)
}

/// Removes each provider scratch in `runs` that a killed run left behind. It costs one
/// directory listing of the leftovers: a finished run removes its own scratch.
fn sweep(runs: &Path) {
    let Ok(entries) = std::fs::read_dir(runs) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| now.duration_since(at).ok())
            .is_some_and(|age| age >= SWEEP_AFTER);
        if old && entry.file_name().to_string_lossy().starts_with("run-") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn run_provider(
    repo: &Repo,
    policy_commit: &str,
    entry: &InputEntry,
    commit: &str,
    tree: &str,
    scratch: &Path,
) -> Result<Version, String> {
    let git = &repo.git;
    let adapter_dir = scratch.join("adapter");
    let tree_dir = scratch.join("tree");
    let home = scratch.join("home");
    let tmp = scratch.join("tmp");
    crate::check::materialize(git, policy_commit, &entry.adapter, &adapter_dir)?;
    crate::check::materialize(git, commit, &[], &tree_dir)?;
    for dir in [&home, &tmp] {
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    }
    let mut words = entry.provider.split_whitespace();
    let program: PathBuf = adapter_dir.join(words.next().unwrap_or_default());
    let args: Vec<&str> = words.collect();
    // § 7: a provider is adapter code and runs where a check runs. The repository, every view
    // and this machine's keys are hidden; its own scratch is all of them it sees.
    let hidden = crate::check::hidden_for(repo, &crate::signing::Keys::load(git));
    let mut cmd = crate::check::sandboxed(
        crate::check::sandbox_kind(),
        &program,
        &args,
        &hidden,
        scratch,
    );
    // The same fixed baseline as a check (`INTERFACES` § 7): nothing from the caller's
    // environment, so an answer depends on the entry and the tree alone.
    cmd.current_dir(&tree_dir)
        .env_clear()
        .env("BAY_ADAPTER_DIR", &adapter_dir)
        .env("HOME", &home)
        .env("TMPDIR", &tmp)
        .env("LANG", "C.UTF-8");
    if !entry.path.is_empty() {
        cmd.env("PATH", entry.path.join(":"));
    }
    let request = json!({"input": entry.name, "tree": tree}).to_string();
    let (status, stdout, stderr) = run_bounded(
        cmd,
        request.as_bytes(),
        Duration::from_secs(entry.timeout_s),
    )
    .map_err(|why| format!("the provider for {}: {why}", entry.name))?;
    if !status.success() {
        return Err(format!(
            "the provider for {} exited with {status}: {}",
            entry.name,
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    let version: Version = serde_json::from_slice(&stdout).map_err(|error| {
        format!(
            "the provider for {} printed no version: {error}",
            entry.name
        )
    })?;
    if version.version.is_empty() {
        return Err(format!(
            "the provider for {} printed an empty version",
            entry.name
        ));
    }
    Ok(version)
}

/// Runs `cmd` with `input` on stdin in a process group of its own, and returns its status,
/// stdout and stderr, all within `timeout`. A child the provider leaves running belongs to the
/// provider: once the provider exits, its group is killed, so no child holds its output open,
/// and the output is waited for no longer than the timeout.
fn run_bounded(
    mut cmd: Command,
    input: &[u8],
    timeout: Duration,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("it did not start: {error}"))?;
    let group = child.id();
    let deadline = Instant::now() + timeout;
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let (sender, outputs) = mpsc::channel();
    let pipes: [(usize, Option<Box<dyn Read + Send>>); 2] = [
        (0, child.stdout.take().map(|pipe| Box::new(pipe) as _)),
        (1, child.stderr.take().map(|pipe| Box::new(pipe) as _)),
    ];
    for (which, pipe) in pipes {
        let mut pipe = pipe.ok_or("an output pipe is missing")?;
        let sender = sender.clone();
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            let _ = sender.send((which, buffer));
        });
    }
    drop(sender);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_group(group);
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("it timed out after {}s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut output = [Vec::new(), Vec::new()];
    let mut received = 0;
    let mut killed = false;
    while received < 2 {
        // A provider whose output is still open shortly after it exited left a child holding
        // it: the group goes, and the output is waited for until the deadline at most.
        let wait = if killed {
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(1))
        } else {
            Duration::from_millis(200)
        };
        match outputs.recv_timeout(wait) {
            Ok((which, buffer)) => {
                output[which] = buffer;
                received += 1;
            }
            Err(mpsc::RecvTimeoutError::Timeout) if !killed => {
                kill_group(group);
                killed = true;
            }
            Err(_) => return Err("a child it left behind held its output open".to_owned()),
        }
    }
    let [stdout, stderr] = output;
    Ok((status, stdout, stderr))
}

/// Kills process group `group`, with a bounded `kill`.
fn kill_group(group: u32) {
    let mut kill = Command::new("kill");
    kill.args(["-KILL", "--", &format!("-{group}")]);
    let _ = crate::git::run_bounded_for(kill, "kill", None, Duration::from_secs(5));
}

/// The version of each of `record`'s named inputs at `base`, under the policy at `base`: what
/// the actor reads in a view of `base` (`observed_versions`). An input whose version is not
/// known there is left out, so it is never counted as observed. The provider runs fresh: an
/// answer in the cache is never what an actor read.
#[must_use]
pub fn observe(repo: &Repo, record: &ChangeRecord, base: &str) -> BTreeMap<String, String> {
    let names = names(&record.declared_inputs);
    let mut observed = BTreeMap::new();
    if names.is_empty() {
        return observed;
    }
    let Ok(Some(policy)) = policy::load_at(&repo.git, base) else {
        return observed;
    };
    for name in names {
        if let Some(entry) = policy.inputs.iter().find(|entry| entry.name == name)
            && let Ok(version) = version_at(repo, base, entry, base, Probe::Fresh)
        {
            observed.insert(name.to_string(), version.version);
        }
    }
    observed
}

/// Where one named input stands for a change (`DESIGN` § 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputState {
    /// No conflict found in what was checked.
    Clear,
    /// Trunk requires a version the change has not observed (`INPUT_UNOBSERVED`).
    Unobserved,
    /// Observed, and no candidate is validated under it yet (`INPUT_CHANGED`).
    Changed,
    /// The required version cannot be established (`COVERAGE_UNKNOWN`): never clear.
    Unknown,
}

/// One named input of a change, judged against a trunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judgement {
    pub input: InputName,
    pub state: InputState,
    pub observed: Option<String>,
    pub required: Option<String>,
    /// The version the validated candidate was checked under.
    pub validated: Option<String>,
    pub phase: Option<Phase>,
    /// How the change's declared paths consume the input, when the provider lists them.
    pub consumer: Option<ConsumerKind>,
    /// Why the coverage is unknown.
    pub why: Option<String>,
    /// Whether trunk's policy names a provider for the input. An input it names whose version
    /// is unknown holds publication back (`COVERAGE_UNKNOWN`): nothing shows the change
    /// observed what trunk requires. One it does not name requires no version.
    pub named: bool,
}

impl Judgement {
    /// The reason code a refusal on this input carries, when it is stale.
    #[must_use]
    pub const fn reason(&self) -> Option<Reason> {
        match self.state {
            InputState::Unobserved => Some(Reason::InputUnobserved),
            InputState::Changed => Some(Reason::InputChanged),
            InputState::Clear | InputState::Unknown => None,
        }
    }

    /// Whether this input invalidates the change.
    #[must_use]
    pub const fn is_stale(&self) -> bool {
        matches!(self.state, InputState::Unobserved | InputState::Changed)
    }

    /// The JSON shape `status` and refusals report: every answer carries its coverage.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "input": self.input,
            "state": self.state,
            "reason": self.reason(),
            "observed": self.observed,
            "required": self.required,
            "validated": self.validated,
            "phase": self.phase,
            "consumer": self.consumer,
            "coverage": if self.state == InputState::Unknown { "unknown" } else { "covered" },
            "why": self.why,
        })
    }
}

/// Every named input of `record` judged against `trunk`: the required version is the
/// provider's answer at `trunk` under the policy at `trunk` (§ 8, never stored).
///
/// # Errors
///
/// A git fault. A provider that gives no version is a judgement of `Unknown`, not an error.
pub fn judge(
    repo: &Repo,
    record: &ChangeRecord,
    trunk: &str,
    probe: Probe,
) -> Result<Vec<Judgement>, String> {
    let names = names(&record.declared_inputs);
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let declared = paths(&record.declared_inputs);
    let policy = match policy::load_at(&repo.git, trunk) {
        Ok(policy) => Ok(policy),
        Err(policy::LoadError::Invalid(why)) => Err(why),
        Err(policy::LoadError::Git(why)) => return Err(why),
    };
    Ok(names
        .into_iter()
        .map(|name| {
            let observed = record.observed_versions.get(name.as_str()).cloned();
            let validated = record
                .validated_candidate
                .as_ref()
                .and_then(|validated| validated.versions.get(name.as_str()).cloned());
            let mut judgement = Judgement {
                input: name.clone(),
                state: InputState::Unknown,
                observed,
                required: None,
                validated,
                phase: None,
                consumer: None,
                why: None,
                named: false,
            };
            let entry = match &policy {
                Ok(Some(policy)) => policy.inputs.iter().find(|entry| entry.name == name),
                Ok(None) => None,
                Err(why) => {
                    judgement.why = Some(why.clone());
                    return judgement;
                }
            };
            let Some(entry) = entry else {
                judgement.why = Some(format!(
                    "the policy at trunk has no [[input]] entry for {name}"
                ));
                return judgement;
            };
            judgement.phase = entry.phase;
            judgement.named = true;
            match version_at(repo, trunk, entry, trunk, probe) {
                Ok(required) => {
                    judgement.consumer = consumer_kind(&required.consumers, &declared);
                    judgement.required = Some(required.version);
                    (judgement.state, judgement.why) =
                        state(repo, record, trunk, entry, &judgement, probe);
                }
                Err(why) => judgement.why = Some(why),
            }
            judgement
        })
        .collect())
}

/// § 8: a historical consumer never blocks; while a contract is in `expand` or `migrate` a
/// consumer stays valid; otherwise an unobserved required version is `Unobserved`, and an
/// observed one that moved since the change started is `Changed` until a candidate is
/// validated under it. Returns the state, and why it is unknown when it is.
fn state(
    repo: &Repo,
    record: &ChangeRecord,
    trunk: &str,
    entry: &InputEntry,
    judgement: &Judgement,
    probe: Probe,
) -> (InputState, Option<String>) {
    if judgement.consumer == Some(ConsumerKind::Historical)
        || matches!(judgement.phase, Some(Phase::Expand | Phase::Migrate))
    {
        return (InputState::Clear, None);
    }
    if judgement.observed != judgement.required {
        return (InputState::Unobserved, None);
    }
    if judgement.validated == judgement.required {
        return (InputState::Clear, None);
    }
    // The version the change was first built on: when trunk still requires it, nothing moved.
    // One that cannot be read is no proof that nothing moved; from the cache alone it is not
    // known either way.
    match version_at(repo, trunk, entry, &record.base_first, probe) {
        Ok(started) if Some(&started.version) == judgement.required.as_ref() => {
            (InputState::Clear, None)
        }
        Err(why) if probe == Probe::Cached => (InputState::Unknown, Some(why)),
        _ => (InputState::Changed, None),
    }
}

/// How the declared paths consume an input: the most binding kind among the provider's
/// consumers that overlap them. A change that declares no paths holds the whole tree.
fn consumer_kind(consumers: &[Consumer], declared: &[String]) -> Option<ConsumerKind> {
    consumers
        .iter()
        .filter(|consumer| {
            declared.is_empty()
                || declared
                    .iter()
                    .any(|path| within(path, &consumer.path) || within(&consumer.path, path))
        })
        .map(|consumer| consumer.kind)
        .max()
}

/// Whether `inner` lies in `outer` (a file, or a directory with or without its `/`).
fn within(outer: &str, inner: &str) -> bool {
    let outer = outer.trim_end_matches('/');
    inner.trim_end_matches('/') == outer || inner.starts_with(&format!("{outer}/"))
}

/// The state a change's judged inputs give it (§ 3.1): `invalidated` while trunk requires a
/// version it has not observed, `repairing` while no candidate is validated under an observed
/// one, `None` when no input is stale.
#[must_use]
pub fn derived_state(judged: &[Judgement]) -> Option<ChangeState> {
    let any = |state| judged.iter().any(|judgement| judgement.state == state);
    if any(InputState::Unobserved) {
        Some(ChangeState::Invalidated)
    } else if any(InputState::Changed) {
        Some(ChangeState::Repairing)
    } else {
        None
    }
}

/// The judgements as the JSON list `status` and refusals carry.
#[must_use]
pub fn to_json<'a>(judged: impl Iterator<Item = &'a Judgement>) -> Value {
    judged.map(Judgement::to_json).collect()
}

/// The required version of each judged input whose version is known: what a candidate checked
/// at that trunk is validated under.
#[must_use]
pub fn required_versions(judged: &[Judgement]) -> BTreeMap<String, String> {
    judged
        .iter()
        .filter_map(|judgement| {
            judgement
                .required
                .clone()
                .map(|version| (judgement.input.to_string(), version))
        })
        .collect()
}

/// The request's `inputs` (`INTERFACES` § 3): `<kind>:<name>@<version>` for each judged input
/// whose required version is known, what the check validates.
#[must_use]
pub fn request_inputs(judged: &[Judgement]) -> Vec<String> {
    required_versions(judged)
        .into_iter()
        .map(|(name, version)| format!("{name}@{version}"))
        .collect()
}

/// The stale inputs of `record` against the local cache of trunk, from local data only: no
/// fetch, no network, and no provider run, only answers already cached (for `bay watch` and
/// the `pre-tool` refusal, which must stay cheap). `Unknown` inputs are not stale; [`judge`]
/// reports them.
///
/// # Errors
///
/// The local cache holds no trunk, or a git fault.
pub fn stale_inputs(repo: &Repo, record: &ChangeRecord) -> Result<Vec<Judgement>, String> {
    let trunk = repo
        .git
        .run(&["rev-parse", "--verify", &repo.trunk_tracking_ref()])
        .map_err(|error| format!("the local cache holds no trunk: {}", error.message()))?;
    Ok(judge(repo, record, &trunk, Probe::Cached)?
        .into_iter()
        .filter(Judgement::is_stale)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{ConsumerKind, DeclaredInput, InputName, consumer_kind, within};

    #[test]
    fn a_named_input_is_kind_colon_name() {
        assert!(InputName::parse("contract:api.orders").is_ok());
        assert!(InputName::parse("config:db-url").is_ok());
        for bad in [
            "api",
            ":api",
            "Contract:api",
            "contract:",
            "contract:a b",
            "1x:a",
        ] {
            assert!(InputName::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_declared_path_with_a_colon_stays_a_path() {
        let declared: Vec<DeclaredInput> =
            serde_json::from_str(r#"["src/a:b", {"input": "contract:api"}]"#).unwrap();
        assert_eq!(declared[0], DeclaredInput::Path("src/a:b".to_owned()));
        assert!(matches!(declared[1], DeclaredInput::Named { .. }));
    }

    #[test]
    fn the_most_binding_overlapping_consumer_decides() {
        let consumers: Vec<super::Consumer> = serde_json::from_str(
            r#"[{"path": "src/", "kind": "executable"}, {"path": "docs/archive/", "kind": "historical"}]"#,
        )
        .unwrap();
        let kind = |paths: &[&str]| {
            let paths: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();
            consumer_kind(&consumers, &paths)
        };
        assert_eq!(kind(&["docs/archive/"]), Some(ConsumerKind::Historical));
        assert_eq!(
            kind(&["docs/archive/old.md"]),
            Some(ConsumerKind::Historical)
        );
        assert_eq!(
            kind(&["docs/archive/", "src/x.rs"]),
            Some(ConsumerKind::Executable)
        );
        assert_eq!(kind(&["docs"]), Some(ConsumerKind::Historical));
        assert_eq!(kind(&[]), Some(ConsumerKind::Executable));
        assert_eq!(kind(&["lib/"]), None);
        assert!(within("src", "src/a.rs") && !within("src", "srcx/a.rs"));
    }
}

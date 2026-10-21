//! The check adapter contract (`INTERFACES` § 3, § 7; `DESIGN` § 7).
//!
//! A check runs on a clean materialization of exactly the candidate, never an agent's view.
//! The adapter paths come from the accepted base, materialized read-only outside the
//! candidate's tree, and the command runs from there with the candidate as its working
//! directory. The request goes on stdin; a `bay.check.result/1` comes back on stdout. An
//! exit code alone is never evidence: anything short of a valid, matching result is `unknown`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;

use crate::clock::now_rfc3339;
use crate::evidence::{CheckId, EVIDENCE_SCHEMA, EvidenceRecord, Producer, Subject, Unit, Verdict};
use crate::git::Git;
use crate::materialize::{Unmade, materialize};
use crate::policy::{CheckEntry, Policy};

/// What is being checked.
pub struct Candidate<'a> {
    pub commit: &'a str,
    pub tree: &'a str,
    pub base: &'a str,
    pub base_tree: &'a str,
    /// The paths the candidate's tree changes against the trunk commit it was merged onto,
    /// sorted: the request's `changed_paths`.
    pub changed_paths: &'a [String],
    /// `<op id>.<attempt>`.
    pub id: &'a str,
    /// How the judging policy reached trunk (`INTERFACES` § 6).
    pub policy_provenance: crate::policy::Provenance,
    /// The version of each named input the candidate is checked under, `<kind>:<name>@<version>`
    /// (`INTERFACES` § 3): the request's `inputs`.
    pub inputs: &'a [String],
}

/// The paths `commit`'s tree adds, modifies or deletes against `base`'s, sorted. Renames are
/// a deletion and an addition; names are read NUL-separated, so Git never quotes them.
///
/// # Errors
///
/// Git cannot compare the two commits.
pub fn changed_paths(git: &Git, base: &str, commit: &str) -> Result<Vec<String>, String> {
    let out = git
        .run(&[
            "diff-tree",
            "-r",
            "-z",
            "--name-only",
            "--no-renames",
            base,
            commit,
        ])
        .map_err(|error| error.message())?;
    let mut paths: Vec<String> = out
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect();
    paths.sort_unstable();
    Ok(paths)
}

/// What bounds one run of the checks. No default: the sandbox is always chosen (ruling R14).
pub struct Limits {
    /// The pre-publication execution budget.
    pub budget: Option<Duration>,
    /// Checks that already finished on this tree under this manifest; not run again.
    pub finished: Vec<CheckId>,
    /// How often to call the heartbeat while a check runs.
    pub heartbeat: Option<Duration>,
    /// Directories the check may neither read nor write: the repository and every view in it.
    /// Its own scratch (the candidate and the adapter) is the only part of them it sees.
    pub hidden: Vec<PathBuf>,
    /// The sandbox the checks run in, and why none runs when there is none (ruling R14).
    pub sandbox: Sandbox,
}

/// What an adapter, a check's or an input provider's, may neither read nor write (§ 7): the
/// repository with every view in it, and every key this machine holds, so code under test can
/// never read a producer key or write a signed record.
#[must_use]
pub fn hidden_for(repo: &crate::repo::Repo, keys: &crate::signing::Keys) -> Vec<PathBuf> {
    let mut hidden = vec![repo.work_root.clone(), repo.common_dir.clone()];
    // A linked worktree bay was started in is the repository's too (a view is already under
    // the common directory).
    if let Ok(top) = Git::new(&repo.opened_in).run(&["rev-parse", "--show-toplevel"]) {
        let top = PathBuf::from(top);
        if !top.starts_with(&repo.common_dir) && !hidden.contains(&top) {
            hidden.push(top);
        }
    }
    let keys = [
        keys.agent.clone(),
        keys.executor.as_ref().map(|signer| signer.key.clone()),
        keys.producer.as_ref().map(|signer| signer.key.clone()),
    ];
    hidden.extend(keys.into_iter().flatten());
    hidden
}

/// The sandbox a check ran in: `sandbox-exec` on macOS, `bwrap` on Linux, or none when this
/// machine has neither (the evidence record says so).
pub(crate) fn sandbox_kind() -> &'static str {
    if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").exists() {
        "sandbox-exec"
    } else if cfg!(target_os = "linux")
        // The probe is the sandbox a check gets, so a `bwrap` that cannot unshare the network
        // or mount a private `/proc` is no sandbox here.
        && Command::new("bwrap")
            .args(sandbox::bwrap_system_args(false))
            .arg("true")
            .output()
            .is_ok_and(|out| out.status.success())
    {
        "bwrap"
    } else {
        "none"
    }
}

/// The remedy for a Linux box whose `bwrap` is installed but cannot build a sandbox because
/// `AppArmor` restricts unprivileged user namespaces (Ubuntu 24.04 and later): `bwrap` is where
/// it is installed, and `restricted` is the contents of
/// `kernel.apparmor_restrict_unprivileged_userns`.
#[must_use]
pub fn apparmor_remedy(bwrap: Option<&Path>, restricted: Option<&str>) -> Option<String> {
    let bwrap = bwrap?;
    restricted
        .is_some_and(|value| value.trim() == "1")
        .then(|| {
            let bwrap = bwrap.display();
            format!(
                "bwrap is installed, but AppArmor restricts unprivileged user namespaces \
             (kernel.apparmor_restrict_unprivileged_userns = 1): add an AppArmor profile for \
             {bwrap} that allows userns (abi <abi/4.0>, profile bwrap {bwrap} \
             flags=(unconfined) {{ userns, }}) in /etc/apparmor.d/bwrap and load it with \
             apparmor_parser -r; or choose to run checks without a sandbox: \
             git config --global {UNSANDBOXED_KEY} none"
            )
        })
}

/// Where this machine says whether `AppArmor` restricts unprivileged user namespaces; in a
/// test build `BAY_TEST_USERNS_RESTRICT` names a file to read instead.
const USERNS_RESTRICT: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";
const TEST_HOOKS: bool = option_env!("BAY_RELEASE_VERSION").is_none();

/// This machine's [`apparmor_remedy`], read from `PATH` and `/proc`.
#[must_use]
pub fn apparmor_remedy_here() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let bwrap = Some(on_path("bwrap")).filter(|path| path.is_absolute())?;
    let file = std::env::var_os("BAY_TEST_USERNS_RESTRICT")
        .filter(|_| TEST_HOOKS)
        .map_or_else(|| PathBuf::from(USERNS_RESTRICT), PathBuf::from);
    let restricted = std::fs::read_to_string(file).ok();
    apparmor_remedy(Some(&bwrap), restricted.as_deref())
}

/// The box's choice to run checks with no sandbox (ruling R14): global Git config, never a
/// repository's, as the update pin is the box's. Its one value is `none`.
pub const UNSANDBOXED_KEY: &str = "bay.executorSandbox";

/// Why a check does not run on a machine with no sandbox (ruling R14).
pub const NO_SANDBOX: &str = "this machine has no check sandbox, so the check did not run; \
     to run checks without one, the box chooses it with git config --global \
     bay.executorSandbox none (each record then says sandbox \"none\")";

/// The sandbox checks run in on this machine, and, where there is none and the box has not
/// chosen `bay.executorSandbox = none`, why no check runs (ruling R14): a check never runs
/// unconfined unless the box chose that.
#[derive(Clone, Debug)]
pub struct Sandbox {
    /// `sandbox-exec`, `bwrap` or `none`: what each record names.
    pub kind: &'static str,
    /// Set when no check may run: each one is then `infra_failure` with this reason.
    pub refusal: Option<String>,
}

impl Sandbox {
    /// This machine's sandbox. Its global Git config is read only when there is none.
    #[must_use]
    pub fn here() -> Self {
        let kind = sandbox_kind();
        if kind == "none" {
            Self::judge(kind, box_chose_unsandboxed())
        } else {
            Self::judge(kind, Ok(false))
        }
    }

    /// The ruling itself, on every platform: with a sandbox the check runs in it; with none it
    /// runs only when the box chose that (`chosen` is `Ok(true)`). An unreadable or unknown
    /// choice is no choice.
    #[must_use]
    pub fn judge(kind: &'static str, chosen: Result<bool, String>) -> Self {
        let refusal = if kind == "none" {
            match chosen {
                Ok(true) => None,
                Ok(false) => Some(NO_SANDBOX.to_owned()),
                Err(why) => Some(format!("{why}; {NO_SANDBOX}")),
            }
        } else {
            None
        };
        Self { kind, refusal }
    }
}

/// Whether this box chose to run checks with no sandbox: `bay.executorSandbox` in global Git
/// config is `none`. Unset is `false`; any other value, or a Git that does not answer, is an
/// error, so the caller refuses.
///
/// # Errors
///
/// A value other than `none`, or a `git config` that fails.
pub fn box_chose_unsandboxed() -> Result<bool, String> {
    let mut cmd = Command::new("git");
    cmd.args(["config", "--global", "--get", UNSANDBOXED_KEY]);
    let output = crate::git::run_bounded(cmd, "git config --global")
        .map_err(|error| format!("{UNSANDBOXED_KEY} could not be read: {}", error.message()))?;
    match output.status.code() {
        // `git config --get` exits 1 for a key that is not set.
        Some(1) => Ok(false),
        Some(0) => match String::from_utf8_lossy(&output.stdout).trim() {
            "none" => Ok(true),
            other => Err(format!(
                "{UNSANDBOXED_KEY} = \"{other}\" is not a choice bay knows; its one value is \
                 \"none\""
            )),
        },
        _ => Err(format!(
            "{UNSANDBOXED_KEY} could not be read: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// The box's declaration of the policy environments its executor serves (`DESIGN` § 7,
/// `INTERFACES` § 7): Git config, one value per environment, global or for one clone, read in
/// the clone that runs the operation. A repository never declares one.
pub const ENVIRONMENT_KEY: &str = "bay.checkEnvironment";

/// The environments this box serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Served {
    /// The box declares nothing: it serves every environment, as every box did before the
    /// declaration existed, so an upgrade stops no executor from running the checks it ran.
    Every,
    /// The box declares these environments and serves exactly them. Empty values are left
    /// out, so a box under maintenance withdraws every environment with one empty value; a
    /// declaration Git cannot read serves nothing.
    Declared(Vec<String>),
}

impl Served {
    /// `bay.checkEnvironment` as `git config --get-all` reads it in `git`'s clone.
    #[must_use]
    pub fn read(git: &Git) -> Self {
        match git.run_status(&["config", "--get-all", ENVIRONMENT_KEY]) {
            // `git config --get-all` exits 1 for a key that is not set.
            Ok((1, _, _)) => Self::Every,
            Ok((0, values, _)) => Self::Declared(
                values
                    .lines()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect(),
            ),
            _ => Self::Declared(Vec::new()),
        }
    }

    /// Whether the box serves `environment`, by its exact name.
    #[must_use]
    pub fn serves(&self, environment: &str) -> bool {
        match self {
            Self::Every => true,
            Self::Declared(declared) => declared.iter().any(|value| value == environment),
        }
    }

    /// `declared`, or `default` when the box declares nothing.
    #[must_use]
    pub const fn source(&self) -> &'static str {
        match self {
            Self::Every => "default",
            Self::Declared(_) => "declared",
        }
    }

    /// The declared values; none for a box that declares nothing.
    #[must_use]
    pub fn declared(&self) -> &[String] {
        match self {
            Self::Every => &[],
            Self::Declared(declared) => declared,
        }
    }
}

/// The environments of `policy`'s checks required of a candidate that changes `changed`, outside
/// `finished`, that `served` does not serve: sorted, each once. A pair with evidence already (a
/// resume, or reuse) needs no worker.
#[must_use]
pub fn unserved(
    policy: &Policy,
    changed: &[String],
    finished: &[CheckId],
    served: &Served,
) -> Vec<String> {
    let mut missing: Vec<String> = policy
        .checks
        .iter()
        // G245: an approval runs on no worker; a person's record answers it.
        .filter(|entry| policy.runs(entry, changed))
        .filter(|entry| !finished.contains(&CheckId::new(&entry.id, &entry.environment)))
        .filter(|entry| !served.serves(&entry.environment))
        .map(|entry| entry.environment.clone())
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// `program` as found on this process's `PATH`, or the bare name when it is not there.
fn on_path(program: &str) -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(program))
                .find(|candidate| candidate.is_file())
        })
        .unwrap_or_else(|| PathBuf::from(program))
}

fn real(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The command that runs `executable` inside the sandbox, an allow-list (G62): it sees its
/// `scratch` (the candidate and the adapter) read-write, the system files of
/// [`sandbox::SYSTEM_DIRS`] and [`sandbox::SYSTEM_ETC`] read-only, and nothing else; any of
/// `hidden` inside those is hidden on top. The network is denied unless `network` (a check's
/// `network = true`, judged at the base). With no sandbox, a `#!` script starts as
/// [`crate::platform::script`] says (on Windows through Git for Windows' `sh.exe`).
///
/// # Errors
///
/// With no sandbox on Windows: a `#!` script bay cannot start ([`crate::platform::script`]).
pub(crate) fn sandboxed(
    kind: &str,
    executable: &Path,
    args: &[&str],
    hidden: &[PathBuf],
    scratch: &Path,
    network: bool,
) -> Result<Command, String> {
    Ok(match kind {
        "sandbox-exec" => {
            let mut cmd = Command::new("/usr/bin/sandbox-exec");
            cmd.arg("-p")
                .arg(sandbox::macos_profile(hidden, scratch, network))
                .arg(executable)
                .args(args);
            cmd
        }
        "bwrap" => {
            // Found on the executor's own `PATH`: the check's cleared environment may not have it.
            let mut cmd = Command::new(on_path("bwrap"));
            cmd.args(sandbox::bwrap_args(hidden, scratch, network));
            cmd.arg("--").arg(executable).args(args);
            cmd
        }
        _ => {
            let mut cmd = crate::platform::script(executable)?;
            cmd.args(args);
            cmd
        }
    })
}

#[derive(Debug, Deserialize)]
struct CheckResult {
    schema: String,
    exercised: Exercised,
    plan: Plan,
    #[serde(default)]
    checks: Vec<ResultUnit>,
}

#[derive(Debug, Deserialize)]
struct Exercised {
    commit: String,
    tree: String,
}

#[derive(Debug, Deserialize)]
struct Plan {
    #[serde(default)]
    selected: u64,
    completed: u64,
    /// `declared_empty` when the adapter's map selects nothing for this candidate (§ 7).
    #[serde(default)]
    selection: Option<String>,
}

/// The one recorded value of `plan.selection` (`INTERFACES` § 7).
pub const DECLARED_EMPTY: &str = "declared_empty";

/// What one check's run returns: its verdict, why it is not `pass`, its units, and its
/// `selection` when the selection was declared empty; or, set in `unavailable`, the adapter's
/// answer that no worker could run it, which observed nothing and is no verdict.
struct Judged {
    verdict: Verdict,
    why: Option<String>,
    units: Vec<Unit>,
    selection: Option<String>,
    unavailable: Option<String>,
}

impl Judged {
    const fn without_units(verdict: Verdict, why: Option<String>) -> Self {
        Self {
            verdict,
            why,
            units: Vec::new(),
            selection: None,
            unavailable: None,
        }
    }
}

/// What a run of the checks returns: the record of each check it ran, and, when the adapter
/// answered that no worker could run a check, that check's environment and the adapter's
/// reason. The run stops there and writes no record for that check (`INTERFACES` § 3).
pub struct Ran {
    pub records: Vec<EvidenceRecord>,
    pub unavailable: Option<(String, String)>,
}

/// The adapter's answer that it could not reach a worker for the check (`INTERFACES` § 3):
/// `{"schema": "bay.check.result/1", "unavailable": "<reason>"}` with no `checks`. Any other
/// result without `checks` is no such answer, and is judged as a result.
fn unavailable_answer(output: &[u8]) -> Option<String> {
    let answer: serde_json::Value = serde_json::from_slice(output).ok()?;
    if answer["schema"] != "bay.check.result/1" || answer.get("checks").is_some() {
        return None;
    }
    answer["unavailable"]
        .as_str()
        .filter(|reason| !reason.is_empty())
        .map(str::to_owned)
}

#[derive(Debug, Deserialize)]
struct ResultUnit {
    id: String,
    verdict: Verdict,
    #[serde(default)]
    seconds: Option<f64>,
    #[serde(default)]
    reason: Option<String>,
}

/// Runs every required check of `policy` on `candidate` that is not in `finished`, hands each
/// record to `keep` as soon as its check ends (so a finished unit survives a worker that dies
/// in a later one, § 7), and returns the records of this run. A check the adapter answers
/// `unavailable` for gets no record, and the run stops there: no worker answers for it.
///
/// # Errors
///
/// The candidate or the adapter cannot be materialized (a fault of bay's own, not a verdict),
/// or `keep` fails.
pub fn run_all(
    git: &Git,
    work_dir: &Path,
    policy: &Policy,
    candidate: &Candidate,
    producer: &str,
    limits: &Limits,
    hooks: &mut Hooks,
) -> Result<Ran, String> {
    let finished = limits.finished.as_slice();
    let sandbox = limits.sandbox.kind;
    // The execution budget runs on the monotonic clock from the start of the checks.
    let started = Instant::now();
    let mut records = Vec::new();
    for (index, entry) in policy.checks.iter().enumerate() {
        let id = CheckId::new(&entry.id, &entry.environment);
        // A quarantined check is not required (ruling 8c), a post-publication one runs after
        // publication (G91), one whose paths the candidate does not change is not required
        // of it (G146), and an approval is answered by a person's record, never run (G245):
        // none of them runs here.
        if finished.contains(&id) || !policy.runs(entry, candidate.changed_paths) {
            continue;
        }
        // Flake-aware retry: a `product_failure` runs again on the identical tree, freshly
        // materialized, up to `retry_on_failure` times, and stops at the first other answer.
        // Every run is its own record, so a failure and a later pass combine to `flaky`.
        for run in 0..=entry.retry_on_failure {
            let (mut judged, ran) = run_once(
                git,
                policy,
                candidate,
                limits,
                hooks,
                (index, entry),
                (work_dir, started),
            )?;
            if let Some(reason) = judged.unavailable.take() {
                return Ok(Ran {
                    records,
                    unavailable: Some((entry.environment.clone(), reason)),
                });
            }
            let mut record = evidence(
                candidate,
                producer,
                policy,
                (index, entry, run),
                judged,
                ran,
                sandbox,
            );
            // NEW-10: a check with `footprint = true` stamps its read set, so a later candidate
            // whose read set is the same can carry this record (`crate::throughput`).
            if let Some(stamp) =
                crate::throughput::stamp(git, entry, candidate.tree, candidate.base_tree)
            {
                record
                    .unknown
                    .insert(crate::throughput::FOOTPRINT.to_owned(), stamp);
            }
            let verdict = record.verdict;
            (hooks.keep)(&mut record)?;
            records.push(record);
            if std::env::var("BAY_TEST_KILL_AT")
                .is_ok_and(|at| at == format!("after-check-{}", index + 1))
            {
                std::process::exit(137);
            }
            if verdict != Verdict::ProductFailure {
                break;
            }
        }
    }
    Ok(Ran {
        records,
        unavailable: None,
    })
}

/// One run of the check `entry`, the `index`th of `policy`, on `candidate`: its tree and adapter
/// freshly materialized under `work_dir` and removed after, within what is left of the budget
/// since `started`. Returns the judgement and when the check itself started, after its
/// materialization.
///
/// # Errors
///
/// The candidate or the adapter cannot be materialized (a fault of bay's own, not a verdict).
fn run_once(
    git: &Git,
    policy: &Policy,
    candidate: &Candidate,
    limits: &Limits,
    hooks: &mut Hooks,
    (index, entry): (usize, &CheckEntry),
    (work_dir, started): (&Path, Instant),
) -> Result<(Judged, Instant), String> {
    let budget = limits.budget;
    let dir = work_dir.join(format!("check-{index}"));
    let _ = std::fs::remove_dir_all(&dir);
    let tree_dir = dir.join("candidate");
    let adapter_dir = dir.join("adapter");
    // R14: with no sandbox and no choice by the box, nothing of the check runs.
    let not_run = match &limits.sandbox.refusal {
        Some(why) => Some(Judged::without_units(
            Verdict::InfraFailure,
            Some(why.clone()),
        )),
        None => prepare(
            git,
            entry,
            candidate,
            (&tree_dir, &adapter_dir),
            budget.map(|budget| budget.saturating_sub(started.elapsed())),
            (&mut *hooks.beat, limits.heartbeat),
        )?,
    };
    let place = Place {
        tree: &tree_dir,
        adapter: &adapter_dir,
        scratch: &dir,
        hidden: &limits.hidden,
        sandbox: limits.sandbox.kind,
    };
    let remaining = budget.map(|budget| budget.saturating_sub(started.elapsed()));
    let ran = Instant::now();
    let judged = match not_run {
        Some(judged) => judged,
        None => within_budget(
            hooks,
            limits.heartbeat,
            entry,
            policy,
            candidate,
            &place,
            remaining,
        ),
    };
    make_writable(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    Ok((judged, ran))
}

/// The evidence record of run `run` of the check `entry`, the `index`th of `policy`, judged
/// `judged` on `candidate` by `producer` in a run that started at `ran` under `sandbox`.
fn evidence(
    candidate: &Candidate,
    producer: &str,
    policy: &Policy,
    (index, entry, run): (usize, &CheckEntry, u32),
    judged: Judged,
    ran: Instant,
    sandbox: &str,
) -> EvidenceRecord {
    EvidenceRecord {
        schema: EVIDENCE_SCHEMA.to_owned(),
        id: run_id(producer, candidate.id, index, run),
        kind: "check".to_owned(),
        subject: Subject {
            tree: candidate.tree.to_owned(),
            commit: candidate.commit.to_owned(),
            base_tree: candidate.base_tree.to_owned(),
            unknown: serde_json::Map::new(),
        },
        inputs: EvidenceRecord::inputs_for(&entry.fixtures),
        check: CheckId::new(&entry.id, &entry.environment),
        policy_revision: policy.revision,
        manifest_digest: policy.manifest_digest.clone(),
        policy_provenance: Some(candidate.policy_provenance.as_str().to_owned()),
        producer: Producer {
            name: producer.to_owned(),
            version: crate::version::BAY.to_owned(),
            unknown: serde_json::Map::new(),
        },
        verdict: judged.verdict,
        at: now_rfc3339(),
        why: judged.why,
        units: judged.units,
        selection: judged.selection,
        sandbox: Some(sandbox.to_owned()),
        seconds: Some((ran.elapsed().as_secs_f64() * 1000.0).round() / 1000.0),
        signature: None,
        unknown: serde_json::Map::new(),
    }
}

/// The evidence id of run `run` of the check at `index` of the policy on `candidate`: the first
/// run keeps the id it always had, and each rerun after a `product_failure` gets its own, so no
/// two runs share an id (G29) and every run stays on the tree.
fn run_id(producer: &str, candidate: &str, index: usize, run: u32) -> String {
    if run == 0 {
        format!("{producer}/{candidate}-{}", index + 1)
    } else {
        format!("{producer}/{candidate}-{}-rerun-{run}", index + 1)
    }
}

/// What a run calls back into: `keep` takes each record as soon as its check ends; `beat`
/// renews the executor's lease while a check runs; `seen`, when set, is shown each exchange
/// with the adapter as it ended (the conformance kit, `bay adapter test check`).
pub struct Hooks<'a> {
    pub keep: &'a mut dyn FnMut(&mut EvidenceRecord) -> Result<(), String>,
    pub beat: &'a mut dyn FnMut(),
    pub seen: Option<&'a mut dyn FnMut(&Exchange)>,
}

/// One exchange with a check adapter, as it ended: the request bay sent, and what came back.
#[derive(Clone, Debug)]
pub struct Exchange {
    pub request: serde_json::Value,
    /// The adapter's exit status; `None` when it did not start or ran past its timeout.
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub elapsed: Duration,
    /// The adapter ran past its timeout and was killed.
    pub timed_out: bool,
    /// Why there is no status.
    pub error: Option<String>,
}

impl Exchange {
    fn new(
        request: &serde_json::Value,
        ran: &Result<(ExitStatus, Vec<u8>, String), String>,
        elapsed: Duration,
        timeout: Duration,
    ) -> Self {
        let (status, stdout, stderr, error) = match ran {
            Ok((status, stdout, stderr)) => (Some(*status), stdout.clone(), stderr.clone(), None),
            Err(why) => (None, Vec::new(), String::new(), Some(why.clone())),
        };
        Self {
            request: request.clone(),
            timed_out: status.is_none() && elapsed >= timeout,
            status,
            stdout,
            stderr,
            elapsed,
            error,
        }
    }
}

/// Runs one check within its own timeout and what is left of the execution budget.
fn within_budget(
    hooks: &mut Hooks,
    heartbeat: Option<Duration>,
    entry: &CheckEntry,
    policy: &Policy,
    candidate: &Candidate,
    place: &Place,
    remaining: Option<Duration>,
) -> Judged {
    let own = Duration::from_secs(entry.timeout_s);
    match remaining {
        Some(remaining) if remaining.is_zero() => Judged::without_units(
            Verdict::Unknown,
            Some("the execution budget was exhausted before this check started".to_owned()),
        ),
        Some(remaining) if remaining < own => {
            let mut judged = run_one(hooks, heartbeat, entry, policy, candidate, place, remaining);
            judged.why = judged.why.map(|why| {
                if why.contains("timed out") {
                    format!("the execution budget was exhausted: {why}")
                } else {
                    why
                }
            });
            judged
        }
        _ => run_one(hooks, heartbeat, entry, policy, candidate, place, own),
    }
}

/// Where one check runs.
struct Place<'a> {
    tree: &'a Path,
    adapter: &'a Path,
    scratch: &'a Path,
    hidden: &'a [PathBuf],
    sandbox: &'a str,
}

/// The `bay.check.request/1` bay sends for `entry` on `candidate` (`INTERFACES` § 3).
fn request(entry: &CheckEntry, policy: &Policy, candidate: &Candidate) -> serde_json::Value {
    let mut requires = json!({"environment": entry.environment});
    if !entry.fixtures.is_empty() {
        // § 3: the fixtures the policy pins, by digest (ruling R18).
        requires["fixtures"] = json!(entry.fixtures);
    }
    json!({
        "schema": "bay.check.request/1",
        "candidate": {"commit": candidate.commit, "tree": candidate.tree},
        "changed_paths": candidate.changed_paths,
        "plan": {"id": entry.id, "version": policy.revision.to_string()},
        "requires": requires,
        "timeout_s": entry.timeout_s,
        "inputs": candidate.inputs,
    })
}

fn run_one(
    hooks: &mut Hooks,
    heartbeat: Option<Duration>,
    entry: &CheckEntry,
    policy: &Policy,
    candidate: &Candidate,
    place: &Place,
    timeout: Duration,
) -> Judged {
    let (tree_dir, adapter_dir) = (place.tree, place.adapter);
    let mut words = entry.command.split_whitespace();
    let executable = adapter_dir.join(words.next().unwrap_or_default());
    let request = request(entry, policy, candidate);
    let args: Vec<&str> = words.collect();
    // Ruling 8b: a named variable the executor does not set is an infrastructure failure;
    // the check never runs without what its policy declares.
    let mut declared = Vec::new();
    for name in &entry.env {
        let Some(value) = std::env::var_os(name) else {
            return Judged::without_units(
                Verdict::InfraFailure,
                Some(format!(
                    "the policy declares {name} in env, and this executor does not set it"
                )),
            );
        };
        declared.push((name, value));
    }
    let scratch_tmp = place.scratch.join("tmp");
    let scratch_home = place.scratch.join("home");
    let _ = std::fs::create_dir_all(&scratch_tmp);
    let _ = std::fs::create_dir_all(&scratch_home);
    let mut cmd = match sandboxed(
        place.sandbox,
        &executable,
        &args,
        place.hidden,
        place.scratch,
        entry.network,
    ) {
        Ok(cmd) => cmd,
        Err(why) => return Judged::without_units(Verdict::InfraFailure, Some(why)),
    };
    // `INTERFACES` § 7: the check starts from a fixed baseline and nothing else, so evidence
    // names everything it could read: `PATH` from the policy, `HOME` and `TMPDIR` inside the
    // sandbox's scratch, `LANG=C.UTF-8`, bay's own `BAY_ADAPTER_DIR`, and the declared names.
    // The working directory is the candidate tree, the root an adapter judges its paths from:
    // the tree's absolute path usually holds a `.git` part, a path inside it never does (G241).
    cmd.current_dir(tree_dir)
        .env_clear()
        .env("BAY_ADAPTER_DIR", adapter_dir)
        .env("HOME", &scratch_home)
        .env("TMPDIR", &scratch_tmp)
        .env("LANG", "C.UTF-8")
        // The policy states `path` or takes its stated default, so `PATH` is always set: a
        // shell never falls back to a search path no policy names.
        .env("PATH", crate::platform::search_path(&entry.path))
        // `policy::load_at` refuses an `env` name of the baseline, so none overrides it.
        .envs(declared);
    let (output, stderr) = match exchange(hooks, cmd, &request, timeout, heartbeat) {
        Ok(output) => output,
        Err(why) => return Judged::without_units(Verdict::Unknown, Some(why)),
    };
    // § 7, G26: a check that reached outside its adapter and candidate is an infrastructure
    // failure, never a pass.
    let reached_out = reaches_out(&stderr, place);
    let outside = || {
        Judged::without_units(
            Verdict::InfraFailure,
            Some("the check read or wrote outside its adapter and candidate".to_owned()),
        )
    };
    if !reached_out && let Some(reason) = unavailable_answer(&output) {
        return Judged {
            unavailable: Some(reason),
            ..Judged::without_units(Verdict::Unknown, None)
        };
    }
    let result: CheckResult = match serde_json::from_slice(&output) {
        Ok(result) => result,
        Err(_) if reached_out => return outside(),
        Err(error) => {
            return Judged::without_units(
                Verdict::Unknown,
                Some(format!("no bay.check.result/1 on stdout: {error}")),
            );
        }
    };
    if reached_out {
        return outside();
    }
    judge(&result, entry, candidate)
}

/// Runs the adapter with `request` on stdin within `timeout`, shows the exchange to
/// `hooks.seen`, and returns its stdout and stderr. A spawn failure, a timeout, or a crash (an
/// exit other than 0 with nothing on stdout or stderr) is an error the caller records as
/// `unknown`.
fn exchange(
    hooks: &mut Hooks,
    cmd: Command,
    request: &serde_json::Value,
    timeout: Duration,
    heartbeat: Option<Duration>,
) -> Result<(Vec<u8>, String), String> {
    let started = Instant::now();
    let ran = run_with_timeout(
        cmd,
        &request.to_string(),
        timeout,
        &mut *hooks.beat,
        heartbeat,
    );
    if let Some(seen) = hooks.seen.as_deref_mut() {
        seen(&Exchange::new(request, &ran, started.elapsed(), timeout));
    }
    let (status, output, stderr) = ran?;
    if !status.success() && output.is_empty() && stderr.is_empty() {
        return Err(format!(
            "the check crashed ({status}) and returned no result"
        ));
    }
    Ok((output, stderr))
}

/// Whether the check's stderr names a hidden path. A path inside the check's own scratch is
/// never one, though the scratch sits under the repository's git directory (G241): a shell's
/// error about the check's own adapter is no reach outside.
fn reaches_out(stderr: &str, place: &Place) -> bool {
    let own = forms(place.scratch);
    place.hidden.iter().any(|hidden| {
        forms(hidden)
            .iter()
            .any(|form| names_path_outside(stderr, form, &own))
    })
}

/// Each form a message may name `path` by: as given, its real path, and, for a path under
/// macOS's `/private/var`, `/private/tmp` or `/private/etc`, the short form through the
/// `/var`, `/tmp` or `/etc` link, which a shell prints when its `TMPDIR` or argument used it.
fn forms(path: &Path) -> Vec<String> {
    let mut forms = Vec::new();
    for given in [path.to_path_buf(), real(path)] {
        let given = given.display().to_string();
        let short = ["/var", "/tmp", "/etc"].iter().find_map(|link| {
            let rest = given.strip_prefix("/private")?.strip_prefix(link)?;
            (rest.is_empty() || rest.starts_with('/')).then(|| format!("{link}{rest}"))
        });
        forms.extend(Some(given).into_iter().chain(short));
    }
    forms.retain(|form| !form.is_empty());
    forms.sort();
    forms.dedup();
    forms
}

/// Whether `text` names `path` or a path inside it: an occurrence of `path` that starts and
/// ends on a path boundary, so `/box/repo` is named by `/box/repo` and `/box/repo/x` but not by
/// its sibling `/box/repo-remote.git` or by `/other/box/repo`.
#[cfg(test)]
fn names_path(text: &str, path: &str) -> bool {
    names_path_outside(text, path, &[])
}

/// [`names_path`], leaving out each mention of `path` that is the start of one of `own`: a
/// path inside the check's own scratch, which may sit under `path`.
fn names_path_outside(text: &str, path: &str, own: &[String]) -> bool {
    // What may stand next to a path in a message: whitespace, quotes and punctuation.
    let delimiter = |c: char| c.is_whitespace() || "'\"`:,;()[]<>=".contains(c);
    if path.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(found) = text[from..].find(path) {
        let start = from + found;
        let end = start + path.len();
        let before = text[..start].chars().next_back();
        let mut after = text[end..].chars();
        let opens = before.is_none_or(delimiter);
        let closes = match after.next() {
            None | Some('/') => true,
            // A full stop ends a sentence, never a name such as `repo.git`.
            Some('.') => after.next().is_none_or(delimiter),
            Some(c) => delimiter(c),
        };
        let inside_own = own
            .iter()
            .any(|own| text[start..].starts_with(own.as_str()));
        if opens && closes && !inside_own {
            return true;
        }
        from = start + text[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// The units as the evidence record keeps them: a time only when it is a real one, a reason
/// only on a unit that did not pass, and none of the adapter's other fields.
fn recorded_units(result: &CheckResult) -> Vec<Unit> {
    result
        .checks
        .iter()
        .map(|unit| Unit {
            id: unit.id.clone(),
            verdict: unit.verdict,
            seconds: unit
                .seconds
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0),
            reason: unit
                .reason
                .clone()
                .filter(|_| unit.verdict != Verdict::Pass),
            unknown: serde_json::Map::new(),
        })
        .collect()
}

/// G240: a reason is shown whole or not at all; one too long to show makes the run an infra
/// failure of the adapter, never a cut message.
fn overlong_reason(result: &CheckResult) -> Option<Judged> {
    let unit = result.checks.iter().find(|unit| {
        unit.reason
            .as_ref()
            .is_some_and(|reason| reason.len() > crate::evidence::REASON_MAX_BYTES)
    })?;
    Some(Judged::without_units(
        Verdict::InfraFailure,
        Some(format!(
            "unit {} sent a reason of {} bytes; at most {} are shown",
            unit.id,
            unit.reason.as_ref().map_or(0, String::len),
            crate::evidence::REASON_MAX_BYTES
        )),
    ))
}

fn judge(result: &CheckResult, entry: &CheckEntry, candidate: &Candidate) -> Judged {
    if result.schema != "bay.check.result/1" {
        return Judged::without_units(Verdict::Unknown, Some(format!("schema {}", result.schema)));
    }
    if result.exercised.commit != candidate.commit || result.exercised.tree != candidate.tree {
        return Judged::without_units(
            Verdict::Unknown,
            Some("the result is for a different commit or tree; rejected whole".to_owned()),
        );
    }
    if let Some(too_long) = overlong_reason(result) {
        return too_long;
    }
    let units = recorded_units(result);
    let with = |verdict: Verdict, why: Option<String>, units: Vec<Unit>| Judged {
        verdict,
        why,
        units,
        selection: None,
        unavailable: None,
    };
    if units
        .iter()
        .any(|unit| unit.verdict == Verdict::ProductFailure)
    {
        return with(Verdict::ProductFailure, None, units);
    }
    // § 7: `declared_empty` is valid only with `selected` = 0 and `expected_minimum` = 0; any
    // other combination is `unknown`.
    let declared = result.plan.selection.as_deref();
    if let Some(selection) = declared
        && (selection != DECLARED_EMPTY || result.plan.selected > 0 || entry.minimum() > 0)
    {
        return with(
            Verdict::Unknown,
            Some(format!(
                "selection {selection} with {} selected under the policy's expected_minimum {}; {DECLARED_EMPTY} needs both at zero",
                result.plan.selected,
                entry.minimum()
            )),
            units,
        );
    }
    let collected = u64::try_from(units.len()).unwrap_or(u64::MAX);
    // A selection of tests with no result for any of them is no empty selection: the units
    // the adapter says it ran are missing, so the run is `unknown`, never `declared_empty`.
    if collected == 0 && result.plan.selected > 0 {
        return with(
            Verdict::Unknown,
            Some(format!(
                "selected {} and reported no unit",
                result.plan.selected
            )),
            units,
        );
    }
    if collected < entry.minimum() {
        return with(
            Verdict::Unknown,
            Some(format!(
                "collected {collected} of the policy's expected_minimum {}",
                entry.minimum()
            )),
            units,
        );
    }
    if result.plan.completed < result.plan.selected {
        return with(
            Verdict::Unknown,
            Some(format!(
                "completed {} of {} selected",
                result.plan.completed, result.plan.selected
            )),
            units,
        );
    }
    if units
        .iter()
        .any(|unit| unit.verdict == Verdict::InfraFailure)
    {
        return with(Verdict::InfraFailure, None, units);
    }
    if units.iter().any(|unit| unit.verdict == Verdict::Unknown) {
        return with(
            Verdict::Unknown,
            Some("a unit has no result".to_owned()),
            units,
        );
    }
    // § 7: a pass on an empty selection under an explicit zero minimum is recorded as
    // `declared_empty`, so it is reported as a coverage gap, never as tested code.
    let empty = declared.is_some() || collected == 0;
    Judged {
        verdict: Verdict::Pass,
        why: None,
        units,
        selection: empty.then(|| DECLARED_EMPTY.to_owned()),
        unavailable: None,
    }
}

/// Runs the adapter with the request on stdin; returns its exit status, stdout and stderr. A
/// spawn failure or a timeout is an error, which the caller records as `unknown`, as it does a
/// crash: an exit other than 0 with nothing on stdout or stderr.
fn run_with_timeout(
    mut cmd: Command,
    input: &str,
    timeout: Duration,
    beat: &mut dyn FnMut(),
    heartbeat: Option<Duration>,
) -> Result<(ExitStatus, Vec<u8>, String), String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("the check did not start: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.as_bytes().to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let mut stdout = child.stdout.take().ok_or("stdout pipe is missing")?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let mut stderr = child.stderr.take().ok_or("stderr pipe is missing")?;
    let errors = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });
    let started = Instant::now();
    let mut last_beat = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if heartbeat.is_some_and(|every| last_beat.elapsed() >= every) {
            beat();
            last_beat = Instant::now();
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("the check timed out after {}s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = reader
        .join()
        .map_err(|_| "the check's output reader failed")?;
    let stderr = String::from_utf8_lossy(&errors.join().unwrap_or_default()).into_owned();
    Ok((status, output, stderr))
}

/// Materializes what `entry` sees of the candidate (its `paths`, or the whole tree) into
/// `tree_dir` and its adapter paths from the base into `adapter_dir`, read-only, both within
/// the entry's `materialize_timeout_s` and what is left of the execution budget (`left`),
/// renewing the lease (`beat`, every `heartbeat`) as it goes. Returns what is on record when
/// the check does not run: `infra_failure` naming the bound when it is exceeded or naming a
/// path of the tree bay does not write, `unknown` when the budget runs out first; never a pass.
///
/// # Errors
///
/// Either tree cannot be materialized for another reason (a fault of bay's own).
fn prepare(
    git: &Git,
    entry: &CheckEntry,
    candidate: &Candidate,
    (tree_dir, adapter_dir): (&Path, &Path),
    left: Option<Duration>,
    (beat, heartbeat): (&mut dyn FnMut(), Option<Duration>),
) -> Result<Option<Judged>, String> {
    let budget_out = |when: &str| {
        Some(Judged::without_units(
            Verdict::Unknown,
            Some(format!("the execution budget was exhausted {when}")),
        ))
    };
    if left.is_some_and(|left| left.is_zero()) {
        return Ok(budget_out("before this check started"));
    }
    let seconds = entry.materialize_timeout_s.0;
    let bound = materialize_bound(seconds);
    let by_budget = left.is_some_and(|left| left < bound);
    let now = Instant::now();
    let within = match left {
        Some(left) if by_budget => left,
        _ => bound,
    };
    // A bound beyond what the clock can hold is never reached: the latest deadline it holds
    // instead, never an overflow.
    let deadline = now
        .checked_add(within)
        .or_else(|| now.checked_add(Duration::from_secs(u64::from(u32::MAX))))
        .unwrap_or(now);
    let mut last_beat = Instant::now();
    let mut tick = || {
        if heartbeat.is_some_and(|every| last_beat.elapsed() >= every) {
            beat();
            last_beat = Instant::now();
        }
    };
    let made = materialize(
        git,
        candidate.commit,
        entry.paths.as_deref(),
        tree_dir,
        deadline,
        &mut tick,
    )
    .and_then(|()| {
        materialize(
            git,
            candidate.base,
            Some(&entry.adapter),
            adapter_dir,
            deadline,
            &mut tick,
        )
    });
    match made {
        Ok(()) => {
            make_read_only(adapter_dir);
            Ok(None)
        }
        Err(Unmade::TimedOut) if by_budget => Ok(budget_out(
            "while materializing the candidate and the adapter; the check did not run",
        )),
        Err(Unmade::TimedOut) => Ok(Some(Judged::without_units(
            Verdict::InfraFailure,
            Some(format!(
                "materializing the candidate and the adapter exceeded the check's materialize_timeout_s ({seconds}s); the check did not run"
            )),
        ))),
        Err(Unmade::Refused(why)) => Ok(Some(Judged::without_units(
            Verdict::InfraFailure,
            Some(format!("{why}; the check did not run")),
        ))),
        Err(Unmade::Failed(why)) => Err(why),
        // G265: a box below its disk floor runs no check; never a pass.
        Err(Unmade::DiskLow(low)) => Ok(Some(Judged::without_units(
            Verdict::InfraFailure,
            Some(format!("{}; the check did not run", low.message())),
        ))),
    }
}

/// `materialize_timeout_s` as a duration. Under test a second of it is a millisecond, so a
/// test can exceed the smallest valid bound without a tree that takes a second to write.
fn materialize_bound(seconds: u64) -> Duration {
    #[cfg(test)]
    if tests::MILLISECOND_BOUNDS.with(std::cell::Cell::get) {
        return Duration::from_millis(seconds);
    }
    Duration::from_secs(seconds)
}

#[cfg(unix)]
fn walk(dir: &Path, mode: u32, files_too: bool) {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, mode, files_too);
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode | 0o111));
        } else if files_too && let Ok(meta) = path.metadata() {
            let exec = meta.permissions().mode() & 0o111;
            let _ = std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode((mode & 0o666) | exec),
            );
        }
    }
}

#[cfg(unix)]
fn make_read_only(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    walk(dir, 0o555, true);
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555));
}

#[cfg(unix)]
fn make_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
    walk(dir, 0o755, false);
    let _ = std::fs::set_permissions(dir.join("adapter"), std::fs::Permissions::from_mode(0o755));
    walk(&dir.join("adapter"), 0o755, false);
}

/// Where there are no Unix modes, the adapter's files are read-only by the file attribute;
/// a directory has no such attribute that stops writes into it.
#[cfg(not(unix))]
fn set_read_only(dir: &Path, read_only: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            set_read_only(&path, read_only);
        } else if let Ok(meta) = path.metadata() {
            let mut permissions = meta.permissions();
            permissions.set_readonly(read_only);
            let _ = std::fs::set_permissions(&path, permissions);
        }
    }
}

#[cfg(not(unix))]
fn make_read_only(dir: &Path) {
    set_read_only(dir, true);
}

#[cfg(not(unix))]
fn make_writable(dir: &Path) {
    set_read_only(dir, false);
}

#[cfg(test)]
mod tests {
    use super::{NO_SANDBOX, Sandbox};

    thread_local! {
        /// Set by a test that needs a bound smaller than one second: then a second of
        /// `materialize_timeout_s` is a millisecond (see `materialize_bound`).
        pub(super) static MILLISECOND_BOUNDS: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }

    /// The `AppArmor` remedy is named only for an installed bwrap under the restriction, and
    /// names that bwrap.
    #[test]
    fn the_apparmor_remedy_is_named_only_where_it_applies() {
        let bwrap = std::path::Path::new("/opt/tools/bwrap");
        let remedy = super::apparmor_remedy(Some(bwrap), Some("1\n")).unwrap();
        assert!(remedy.contains("userns"), "{remedy}");
        assert!(
            remedy.contains("profile bwrap /opt/tools/bwrap"),
            "{remedy}"
        );
        assert!(remedy.contains(super::UNSANDBOXED_KEY), "{remedy}");
        assert!(super::apparmor_remedy(Some(bwrap), Some("0\n")).is_none());
        assert!(
            super::apparmor_remedy(None, Some("1\n")).is_none(),
            "install bwrap first"
        );
        assert!(super::apparmor_remedy(Some(bwrap), None).is_none());
    }

    /// § 7, G26: a mention of a hidden path that is the check's own scratch (which sits under
    /// the repository's git directory) is no reach outside; any other mention still is.
    #[test]
    fn the_checks_own_scratch_is_no_reach_outside() {
        let hidden = "/box/repo";
        let own = vec!["/box/repo/.git/bay/check/c1/check-0".to_owned()];
        let names = |text: &str| super::names_path_outside(text, hidden, &own);
        assert!(!names(
            "sh: /box/repo/.git/bay/check/c1/check-0/adapter/scripts/bay-check: not found"
        ));
        assert!(names("sh: .: cannot open /box/repo/scripts/helper.sh"));
        assert!(names(
            "/box/repo/.git/bay/check/c1/check-0/adapter: ok; then /box/repo/secret: denied"
        ));
        assert!(
            names("/box/repo/.git/config: denied"),
            "the rest of .git stays hidden"
        );
    }

    /// § 7, G26 on macOS: a hidden path under `/private/var` is also named by its `/var` form,
    /// which is how a shell prints it; a path that only shares the prefix gets no short form.
    #[test]
    fn a_private_path_is_also_named_by_its_short_form() {
        let forms = |path: &str| super::forms(std::path::Path::new(path));
        let hidden = forms("/private/var/folders/x/T/box/repo");
        assert!(
            hidden.contains(&"/var/folders/x/T/box/repo".to_owned()),
            "{hidden:?}"
        );
        assert!(hidden.contains(&"/private/var/folders/x/T/box/repo".to_owned()));
        assert!(forms("/private/tmp").contains(&"/tmp".to_owned()));
        assert!(!forms("/private/variable/x").contains(&"/variable/x".to_owned()));
        assert_eq!(forms("/box/repo"), vec!["/box/repo".to_owned()]);
    }

    /// § 7, G26: a hidden path is named by whole components only. A sibling that shares its
    /// prefix (`repo` and `repo-remote.git`) is not the hidden path.
    #[test]
    fn a_hidden_path_is_named_by_whole_components_only() {
        let hidden = "/box/repo";
        for named in [
            "cat: /box/repo: Permission denied",
            "cat: '/box/repo/secret.txt': No such file or directory",
            "fatal: not a git repository: /box/repo.",
            "/box/repo",
            "open(\"/box/repo/.git\")",
        ] {
            assert!(super::names_path(named, hidden), "{named}");
        }
        for other in [
            "To /box/repo-remote.git",
            "fatal: '/box/repo-remote.git' does not appear to be a git repository",
            "/box/repo.git",
            "/box/repository",
            "/other/box/repo",
            "/box/repo2/x /box/repos",
        ] {
            assert!(!super::names_path(other, hidden), "{other}");
        }
        // The first occurrence is a sibling and a later one is the path itself.
        assert!(super::names_path(
            "/box/repo-remote.git and /box/repo/x",
            hidden
        ));
    }

    #[test]
    fn no_sandbox_refuses_unless_the_box_chose_none() {
        // R14 on every platform: no sandbox and no choice is refused.
        let refused = Sandbox::judge("none", Ok(false));
        assert_eq!(refused.kind, "none");
        assert_eq!(refused.refusal.as_deref(), Some(NO_SANDBOX));
        // An unreadable or unknown choice is no choice, and the refusal says why.
        let unknown = Sandbox::judge("none", Err("bay.executorSandbox = \"off\"".to_owned()));
        let why = unknown.refusal.unwrap();
        assert!(why.contains("\"off\"") && why.contains(NO_SANDBOX), "{why}");
        // The box's `bay.executorSandbox = none` runs the check, recorded as `none`.
        assert!(Sandbox::judge("none", Ok(true)).refusal.is_none());
        // With a sandbox the choice does not matter.
        assert!(Sandbox::judge("bwrap", Ok(false)).refusal.is_none());
        assert!(
            Sandbox::judge("sandbox-exec", Err(String::new()))
                .refusal
                .is_none()
        );
    }

    /// Ruling R14 on a real run: an adapter that passes, run where there is no sandbox and the
    /// box did not choose `bay.executorSandbox = none`, is `infra_failure`, never a pass;
    /// with that choice the same adapter passes and its record says `none`. The adapter is a
    /// shell script, so this runs where a shell runs adapters (plan W5 brings `sh.exe`).
    #[cfg(unix)]
    #[test]
    fn a_passing_adapter_with_no_sandbox_is_infra_failure() {
        let chose_none = unix::run_under(Sandbox::judge("none", Ok(true)));
        assert_eq!(
            chose_none.verdict,
            crate::evidence::Verdict::Pass,
            "{:?}",
            chose_none.why
        );
        assert_eq!(chose_none.sandbox.as_deref(), Some("none"));

        let refused = unix::run_under(Sandbox::judge("none", Ok(false)));
        assert_eq!(refused.verdict, crate::evidence::Verdict::InfraFailure);
        assert_eq!(refused.why.as_deref(), Some(NO_SANDBOX));
        assert_eq!(refused.sandbox.as_deref(), Some("none"));
        assert_eq!(refused.units.len(), 0);
    }

    #[cfg(unix)]
    mod unix {
        use super::super::{Candidate, Hooks, Limits, Sandbox, run_all};
        use crate::evidence::EvidenceRecord;
        use crate::git::Git;
        use std::os::unix::fs::PermissionsExt as _;

        const POLICY: &str = "revision = 1\n\n[[check]]\nid = \"tests\"\nenvironment = \"local\"\n\
            command = \"scripts/bay-check\"\nadapter = [\"scripts/bay-check\"]\n\
            expected_minimum = 1\ntimeout_s = 60\n";

        const ADAPTER: &str = r#"#!/bin/sh
req=$(cat)
commit=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"}.*/\1/p')
tree=$(printf '%s' "$req" | sed -n 's/.*"candidate":{"commit":"\([0-9a-f]*\)","tree":"\([0-9a-f]*\)"}.*/\2/p')
printf '{"schema":"bay.check.result/1","exercised":{"commit":"%s","tree":"%s"},"plan":{"selected":1,"completed":1},"checks":[{"id":"t1","verdict":"pass"}]}' "$commit" "$tree"
"#;

        /// A repository whose one required check passes every candidate it is given.
        fn passing_repo() -> (tempfile::TempDir, Git, String, String) {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::create_dir_all(root.join(".bay")).unwrap();
            std::fs::create_dir_all(root.join("scripts")).unwrap();
            std::fs::write(root.join(".bay/policy.toml"), POLICY).unwrap();
            let adapter = root.join("scripts/bay-check");
            std::fs::write(&adapter, ADAPTER).unwrap();
            std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
            let git = Git::new(root);
            git.run(&["init", "-q"]).unwrap();
            git.run(&["add", "-A"]).unwrap();
            git.run(&[
                "-c",
                "user.name=bay",
                "-c",
                "user.email=bay@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "seed",
            ])
            .unwrap();
            let commit = git.run(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();
            let tree = git.run(&["rev-parse", "HEAD^{tree}"]).unwrap();
            (dir, git, commit, tree.trim().to_owned())
        }

        /// The one record of running the passing check under `sandbox`.
        pub(super) fn run_under(sandbox: Sandbox) -> EvidenceRecord {
            let (dir, git, commit, tree) = passing_repo();
            let policy = crate::policy::load_at(&git, &commit).unwrap().unwrap();
            let candidate = Candidate {
                commit: &commit,
                tree: &tree,
                base: &commit,
                base_tree: &tree,
                changed_paths: &[],
                id: "op.1",
                policy_provenance: crate::policy::Provenance::Bay,
                inputs: &[],
            };
            let limits = Limits {
                budget: None,
                finished: Vec::new(),
                heartbeat: None,
                hidden: Vec::new(),
                sandbox,
            };
            let mut keep = |_: &mut EvidenceRecord| Ok(());
            let mut records = run_all(
                &git,
                &dir.path().join("scratch"),
                &policy,
                &candidate,
                "local",
                &limits,
                &mut Hooks {
                    keep: &mut keep,
                    beat: &mut || {},
                    seen: None,
                },
            )
            .unwrap()
            .records;
            assert_eq!(records.len(), 1);
            records.remove(0)
        }
    }
}

/// Materialization through a whole run: `paths`, `materialize_timeout_s`, a large tree.
#[cfg(all(test, unix))]
mod materializing;

pub mod sandbox;

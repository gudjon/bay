//! The check adapter contract (`INTERFACES` § 3, § 7; `DESIGN` § 7).
//!
//! A check runs on a clean materialization of exactly the candidate, never an agent's view.
//! The adapter paths come from the accepted base, materialized read-only outside the
//! candidate's tree, and the command runs from there with the candidate as its working
//! directory. The request goes on stdin; a `bay.check.result/1` comes back on stdout. An
//! exit code alone is never evidence: anything short of a valid, matching result is `unknown`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
        && Command::new("bwrap")
            .args(["--dev-bind", "/", "/", "true"])
            .output()
            .is_ok_and(|out| out.status.success())
    {
        "bwrap"
    } else {
        "none"
    }
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

/// The command that runs `executable` inside the sandbox: `hidden` unreadable and unwritable,
/// except `scratch`, which holds this check's candidate and adapter.
pub(crate) fn sandboxed(
    kind: &str,
    executable: &Path,
    args: &[&str],
    hidden: &[PathBuf],
    scratch: &Path,
) -> Command {
    match kind {
        "sandbox-exec" => {
            let quote = |path: &Path| {
                format!(
                    "\"{}\"",
                    real(path).display().to_string().replace('"', "\\\"")
                )
            };
            let denied: Vec<String> = hidden
                .iter()
                .map(|path| {
                    if path.is_file() {
                        format!("(literal {})", quote(path))
                    } else {
                        format!("(subpath {})", quote(path))
                    }
                })
                .collect();
            let profile = format!(
                "(version 1)(allow default)(deny file-read* file-write* {})(allow file-read* file-write* (subpath {}))",
                denied.join(" "),
                quote(scratch)
            );
            let mut cmd = Command::new("/usr/bin/sandbox-exec");
            cmd.arg("-p").arg(profile).arg(executable).args(args);
            cmd
        }
        "bwrap" => {
            // Found on the executor's own `PATH`: the check's cleared environment may not have it.
            let mut cmd = Command::new(on_path("bwrap"));
            cmd.args(["--dev-bind", "/", "/"]);
            for path in hidden {
                if path.is_file() {
                    cmd.arg("--ro-bind").arg("/dev/null").arg(real(path));
                } else {
                    cmd.arg("--tmpfs").arg(real(path));
                }
            }
            let scratch = real(scratch);
            cmd.arg("--bind").arg(&scratch).arg(&scratch);
            cmd.arg("--").arg(executable).args(args);
            cmd
        }
        _ => {
            let mut cmd = Command::new(executable);
            cmd.args(args);
            cmd
        }
    }
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
/// `selection` when the selection was declared empty.
struct Judged {
    verdict: Verdict,
    why: Option<String>,
    units: Vec<Unit>,
    selection: Option<String>,
}

impl Judged {
    const fn without_units(verdict: Verdict, why: Option<String>) -> Self {
        Self {
            verdict,
            why,
            units: Vec::new(),
            selection: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResultUnit {
    id: String,
    verdict: Verdict,
    #[serde(default)]
    seconds: Option<f64>,
}

/// Runs every required check of `policy` on `candidate` that is not in `finished`, hands each
/// record to `keep` as soon as its check ends (so a finished unit survives a worker that dies
/// in a later one, § 7), and returns the records of this run.
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
) -> Result<Vec<EvidenceRecord>, String> {
    let (budget, finished) = (limits.budget, limits.finished.as_slice());
    let sandbox = limits.sandbox.kind;
    // The execution budget runs on the monotonic clock from the start of the checks.
    let started = Instant::now();
    let mut records = Vec::new();
    for (index, entry) in policy.checks.iter().enumerate() {
        let id = CheckId::new(&entry.id, &entry.environment);
        // A quarantined check is not required (ruling 8c): it does not run.
        if finished.contains(&id) || policy.is_quarantined(entry) {
            continue;
        }
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
            sandbox,
        };
        let remaining = budget.map(|budget| budget.saturating_sub(started.elapsed()));
        let ran = Instant::now();
        let Judged {
            verdict,
            why,
            units,
            selection,
        } = match not_run {
            Some(judged) => judged,
            None => within_budget(
                hooks.beat,
                limits.heartbeat,
                entry,
                policy,
                candidate,
                &place,
                remaining,
            ),
        };
        let mut record = EvidenceRecord {
            schema: EVIDENCE_SCHEMA.to_owned(),
            id: format!("{producer}/{}-{}", candidate.id, index + 1),
            kind: "check".to_owned(),
            subject: Subject {
                tree: candidate.tree.to_owned(),
                commit: candidate.commit.to_owned(),
                base_tree: candidate.base_tree.to_owned(),
                unknown: serde_json::Map::new(),
            },
            inputs: EvidenceRecord::inputs_for(&entry.fixtures),
            check: id,
            policy_revision: policy.revision,
            manifest_digest: policy.manifest_digest.clone(),
            policy_provenance: Some(candidate.policy_provenance.as_str().to_owned()),
            producer: Producer {
                name: producer.to_owned(),
                version: crate::version::BAY.to_owned(),
                unknown: serde_json::Map::new(),
            },
            verdict,
            at: now_rfc3339(),
            why,
            units,
            selection,
            sandbox: Some(sandbox.to_owned()),
            seconds: Some((ran.elapsed().as_secs_f64() * 1000.0).round() / 1000.0),
            signature: None,
            unknown: serde_json::Map::new(),
        };
        make_writable(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        (hooks.keep)(&mut record)?;
        records.push(record);
        if std::env::var("BAY_TEST_KILL_AT")
            .is_ok_and(|at| at == format!("after-check-{}", index + 1))
        {
            std::process::exit(137);
        }
    }
    Ok(records)
}

/// What a run calls back into: `keep` takes each record as soon as its check ends; `beat`
/// renews the executor's lease while a check runs.
pub struct Hooks<'a> {
    pub keep: &'a mut dyn FnMut(&mut EvidenceRecord) -> Result<(), String>,
    pub beat: &'a mut dyn FnMut(),
}

/// Runs one check within its own timeout and what is left of the execution budget.
fn within_budget(
    beat: &mut dyn FnMut(),
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
            let mut judged = run_one(beat, heartbeat, entry, policy, candidate, place, remaining);
            judged.why = judged.why.map(|why| {
                if why.contains("timed out") {
                    format!("the execution budget was exhausted: {why}")
                } else {
                    why
                }
            });
            judged
        }
        _ => run_one(beat, heartbeat, entry, policy, candidate, place, own),
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

fn run_one(
    beat: &mut dyn FnMut(),
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
    let mut requires = json!({"environment": entry.environment});
    if !entry.fixtures.is_empty() {
        // § 3: the fixtures the policy pins, by digest (ruling R18).
        requires["fixtures"] = json!(entry.fixtures);
    }
    let request = json!({
        "schema": "bay.check.request/1",
        "candidate": {"commit": candidate.commit, "tree": candidate.tree},
        "changed_paths": candidate.changed_paths,
        "plan": {"id": entry.id, "version": policy.revision.to_string()},
        "requires": requires,
        "timeout_s": entry.timeout_s,
        "inputs": candidate.inputs,
    });
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
    let mut cmd = sandboxed(
        place.sandbox,
        &executable,
        &args,
        place.hidden,
        place.scratch,
    );
    // `INTERFACES` § 7: the check starts from a fixed baseline and nothing else, so evidence
    // names everything it could read: `PATH` from the policy, `HOME` and `TMPDIR` inside the
    // sandbox's scratch, `LANG=C.UTF-8`, bay's own `BAY_ADAPTER_DIR`, and the declared names.
    cmd.current_dir(tree_dir)
        .env_clear()
        .env("BAY_ADAPTER_DIR", adapter_dir)
        .env("HOME", &scratch_home)
        .env("TMPDIR", &scratch_tmp)
        .env("LANG", "C.UTF-8")
        // The policy states `path` or takes its stated default, so `PATH` is always set: a
        // shell never falls back to a search path no policy names.
        .env("PATH", entry.path.join(":"))
        // `policy::load_at` refuses an `env` name of the baseline, so none overrides it.
        .envs(declared);
    let (output, stderr) =
        match run_with_timeout(cmd, &request.to_string(), timeout, beat, heartbeat) {
            Ok(output) => output,
            Err(why) => return Judged::without_units(Verdict::Unknown, Some(why)),
        };
    // § 7, G26: a check that reached outside its adapter and candidate is an infrastructure
    // failure, never a pass.
    let reached_out = place.hidden.iter().any(|hidden| {
        stderr.contains(&hidden.display().to_string())
            || stderr.contains(&real(hidden).display().to_string())
    });
    let outside = || {
        Judged::without_units(
            Verdict::InfraFailure,
            Some("the check read or wrote outside its adapter and candidate".to_owned()),
        )
    };
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
    let units: Vec<Unit> = result
        .checks
        .iter()
        .map(|unit| Unit {
            id: unit.id.clone(),
            verdict: unit.verdict,
            seconds: unit
                .seconds
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0),
            unknown: serde_json::Map::new(),
        })
        .collect();
    let with = |verdict: Verdict, why: Option<String>, units: Vec<Unit>| Judged {
        verdict,
        why,
        units,
        selection: None,
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
        && (selection != DECLARED_EMPTY || result.plan.selected > 0 || entry.expected_minimum > 0)
    {
        return with(
            Verdict::Unknown,
            Some(format!(
                "selection {selection} with {} selected under the policy's expected_minimum {}; {DECLARED_EMPTY} needs both at zero",
                result.plan.selected, entry.expected_minimum
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
    if collected < entry.expected_minimum {
        return with(
            Verdict::Unknown,
            Some(format!(
                "collected {collected} of the policy's expected_minimum {}",
                entry.expected_minimum
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
    }
}

/// Runs the adapter with the request on stdin; returns its stdout. A spawn failure, a
/// timeout or a crash is an error, which the caller records as `unknown`.
fn run_with_timeout(
    mut cmd: Command,
    input: &str,
    timeout: Duration,
    beat: &mut dyn FnMut(),
    heartbeat: Option<Duration>,
) -> Result<(Vec<u8>, String), String> {
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
    if !status.success() && output.is_empty() && stderr.is_empty() {
        return Err(format!(
            "the check crashed ({status}) and returned no result"
        ));
    }
    Ok((output, stderr))
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
                },
            )
            .unwrap();
            assert_eq!(records.len(), 1);
            records.remove(0)
        }
    }
}

/// Materialization through a whole run: `paths`, `materialize_timeout_s`, a large tree.
#[cfg(all(test, unix))]
mod materializing;

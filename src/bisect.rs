//! G244d: culprit finding on a red post check, by first-parent bisection with a rerun at the
//! suspect (the prior art: a trunk-wide test service that finds the breaking change by
//! bisecting the landed range, and runs the suspect again before it names it).
//!
//! A post check (`stage = "post"`, G91) that tests only some trunk tips leaves a [`Suspect`]
//! range: the landings after its newest pass up to its first failure. Off by default, with
//! `[executor] bisect_post = true` in `.bay/config.toml` at trunk, an executor pass narrows it:
//!
//! - Only a red check whose range holds more than one landing and has a pass below it, on a
//!   box that serves the check's environment (`bay.checkEnvironment`), and only when the
//!   check's `producers` list, if any, names this box's producer.
//! - It runs the check on the range's midpoint landing, first parent, through the same adapter
//!   and sandbox path as every check (`crate::check::run_all`): the policy and adapter at the
//!   landing's first parent, the landing's own tree, every path required. Each run is ordinary
//!   post evidence, signed by this box's producer key outside the sandbox (`DESIGN` § 7) and
//!   pushed on its notes ref; its body carries `bisect`, the range it was run for.
//! - A failure keeps the older half, a pass the newer one. Anything else is no verdict: the
//!   bisect stops `undecided`, and a later pass never runs that landing again.
//! - At one landing left, it runs the check there once more. A second failure confirms it: the
//!   pass below and the failure pin it, so the red judgement (`crate::red`) names it `culprit`
//!   and it owns the red. A pass makes that landing `flaky`, which decides nothing: the range
//!   widens again, and no culprit is named.
//! - At most ⌈log2(n)⌉ + 1 runs for a range of n landings.
//!
//! Nothing is stored but the evidence: the culprit is judged from the records on each read.

use serde_json::{Value, json};

use crate::check::{self, Candidate};
use crate::evidence::{self, CheckId, EvidenceRecord, Verdict};
use crate::git::Git;
use crate::red::{Mark, Red, Suspect};
use crate::registry::Registry;
use crate::repo::Repo;
use crate::signing::{Keys, Signer};

/// The [`run`] outcome when the range narrowed to one landing that failed twice.
pub const CULPRIT: &str = "culprit";

/// What this executor pass did for each red post check over more than one landing, or `None`
/// when `[executor] bisect_post` is off or no such check is red. A fault is reported, never
/// fatal to the pass: the queue does not wait on culprit finding.
#[must_use]
pub fn run(repo: &Repo) -> Option<Value> {
    if !repo.config.bisect_post() {
        return None;
    }
    match all(repo) {
        Ok(reports) if reports.is_empty() => None,
        Ok(reports) => Some(Value::Array(reports)),
        Err(why) => Some(json!({"error": why})),
    }
}

/// The largest number of check runs one bisect of `landings` landings makes: ⌈log2(n)⌉ probes
/// and one rerun at the suspect.
#[must_use]
pub fn bound(landings: usize) -> usize {
    probes(landings) + 1
}

/// ⌈log2(n)⌉: the probes that narrow `n` landings, the newest known to fail, to one.
fn probes(landings: usize) -> usize {
    let below = landings.saturating_sub(1);
    usize::try_from(below.bit_width()).unwrap_or(usize::MAX)
}

fn all(repo: &Repo) -> Result<Vec<Value>, String> {
    let git = &repo.git;
    let keys = Keys::load(git);
    repo.fetch_registry();
    repo.fetch_evidence(&keys);
    let registry = Registry::current(git)?;
    let trunk = repo.fetch_trunk().map_err(|error| error.message())?;
    let judgement = crate::red::state(git, &trunk, registry.as_ref())?;
    let served = check::Served::read(git);
    let mut reports = Vec::new();
    for red in &judgement.red {
        let Some(suspect) = red
            .suspect
            .as_ref()
            .filter(|suspect| suspect.landings.len() > 1)
        else {
            continue;
        };
        if suspect.passed.is_none() {
            reports.push(not_run(
                red,
                "no landing in the window passes the check, so the range has no good end",
                &[],
            ));
            continue;
        }
        if !served.serves(&red.environment) {
            reports.push(not_run(
                red,
                "this box does not serve the check's environment",
                &[],
            ));
            continue;
        }
        let writer = Writer::of(&keys, registry.as_ref())?;
        reports.push(bisect(repo, &keys, &writer, red, suspect)?);
    }
    Ok(reports)
}

/// The producer this box records as, and its signer when the repository has a registry.
struct Writer {
    name: String,
    signer: Option<Signer>,
}

impl Writer {
    fn of(keys: &Keys, registry: Option<&Registry>) -> Result<Self, String> {
        let signer = match (registry, &keys.producer) {
            (None, _) => None,
            (Some(_), Some(signer)) => Some(signer.clone()),
            (Some(_), None) => {
                return Err(
                    "the repository has a key registry and this machine has no producer key (git config bay.producerKey)"
                        .to_owned(),
                );
            }
        };
        Ok(Self {
            name: keys.producer_name.clone(),
            signer,
        })
    }

    /// This producer's records of `check` on `tree`.
    fn records(&self, git: &Git, tree: &str, check: &CheckId) -> Result<usize, String> {
        Ok(evidence::producer_records(git, &self.name, tree)?
            .iter()
            .filter(|record| record.kind == evidence::KIND_CHECK && &record.check == check)
            .count())
    }
}

/// Bisects `red` over `suspect`'s landings, oldest first, the newest known to fail.
fn bisect(
    repo: &Repo,
    keys: &Keys,
    writer: &Writer,
    red: &Red,
    suspect: &Suspect,
) -> Result<Value, String> {
    let git = &repo.git;
    let id = CheckId::new(&red.check, &red.environment);
    let mut range: Vec<Mark> = suspect.landings.clone();
    let bound = bound(range.len());
    let probes = probes(range.len());
    let mut tested: Vec<Value> = Vec::new();
    let mut stopped = None;
    while range.len() > 1 && tested.len() < probes {
        // The newest landing of the range already failed: the midpoint of the others.
        let at = (range.len() - 1) / 2;
        let mark = range[at].clone();
        // A landing this box already ran that still decides nothing is not run again.
        if writer.records(git, &tree_of(git, &mark.landing)?, &id)? > 0 {
            stopped = Some("undecided");
            break;
        }
        let verdict = match run_at(repo, keys, writer, red, suspect, &mark.landing)? {
            Ok(verdict) => verdict,
            Err(why) => return Ok(not_run(red, &why, &tested)),
        };
        tested.push(tested_json(&mark, verdict));
        match verdict {
            Verdict::ProductFailure => range.truncate(at + 1),
            Verdict::Pass => range = range.split_off(at + 1),
            Verdict::InfraFailure | Verdict::Unknown => {
                stopped = Some("undecided");
                break;
            }
        }
    }
    let outcome = match stopped {
        Some(outcome) => outcome,
        None if range.len() > 1 => "bounded",
        None => {
            // One rerun at the suspect rules out a flake before it is named.
            let mark = range[0].clone();
            let verdict = match run_at(repo, keys, writer, red, suspect, &mark.landing)? {
                Ok(verdict) => verdict,
                Err(why) => return Ok(not_run(red, &why, &tested)),
            };
            tested.push(tested_json(&mark, verdict));
            match verdict {
                Verdict::ProductFailure => CULPRIT,
                Verdict::Pass => "flaky",
                Verdict::InfraFailure | Verdict::Unknown => "undecided",
            }
        }
    };
    let culprit = (outcome == CULPRIT).then(|| mark_json(&range[0]));
    Ok(json!({
        "check": red.check,
        "environment": red.environment,
        "outcome": outcome,
        "culprit": culprit,
        "tested": tested,
        "runs": tested.len(),
        "bound": bound,
    }))
}

/// Runs `red`'s post check on landing `commit` and records its verdict as this box's post
/// evidence on the landing's tree, pushed. `Ok(Err(why))` when it cannot run here.
fn run_at(
    repo: &Repo,
    keys: &Keys,
    writer: &Writer,
    red: &Red,
    suspect: &Suspect,
    commit: &str,
) -> Result<Result<Verdict, String>, String> {
    let git = &repo.git;
    let id = CheckId::new(&red.check, &red.environment);
    let base = rev(git, &format!("{commit}^1"))?;
    let tree = tree_of(git, commit)?;
    let base_tree = tree_of(git, &base)?;
    let only = match alone(git, writer, &id, &base)? {
        Ok(only) => only,
        Err(why) => return Ok(Err(why)),
    };
    let changed = check::changed_paths(git, &base, commit)?;
    let run = writer.records(git, &tree, &id)? + 1;
    let short = commit.get(..12).unwrap_or(commit);
    let candidate_id = format!("bisect-{short}-{run}");
    let scratch = repo
        .common_dir
        .join("bay")
        .join("check")
        .join(&candidate_id);
    let range = json!({
        "last_passed": suspect.passed.as_ref().map(|mark| mark.landing.clone()),
        "first_failed": suspect.failed.landing,
    });
    let mut keep = |record: &mut EvidenceRecord| -> Result<(), String> {
        // Post evidence, as a post producer writes it: no pre-publication provenance.
        record.policy_provenance = None;
        record.unknown.insert("bisect".to_owned(), range.clone());
        // The producer signs outside the check's sandbox, after reading its output (§ 7).
        if let Some(signer) = &writer.signer {
            record.sign(&signer.key)?;
        }
        evidence::append(git, &writer.name, record)
    };
    let ran = check::run_all(
        git,
        &scratch,
        &only,
        &Candidate {
            commit,
            tree: &tree,
            base: &base,
            base_tree: &base_tree,
            onto: &base,
            change: None,
            changed_paths: &changed,
            id: &candidate_id,
            policy_provenance: crate::policy::Provenance::Bay,
            inputs: &[],
        },
        &writer.name,
        &check::Limits {
            budget: None,
            finished: Vec::new(),
            heartbeat: None,
            hidden: check::hidden_for(repo, keys),
            sandbox: check::Sandbox::here(),
            // Bisection asks whether each commit fails, whole: no base excuses a failure.
            baseline: None,
            parallel: 1,
        },
        &mut check::Hooks {
            keep: &mut keep,
            beat: &mut || {},
            seen: None,
        },
    );
    let _ = std::fs::remove_dir_all(&scratch);
    let ran = ran?;
    if let Some((_, reason)) = ran.unavailable {
        return Ok(Err(format!("no worker answers the check: {reason}")));
    }
    let Some(verdict) = ran.records.first().map(|record| record.verdict) else {
        return Ok(Err("the check wrote no record".to_owned()));
    };
    let notes = evidence::notes_ref(&writer.name);
    git.run(&["push", "-q", repo.remote(), &format!("{notes}:{notes}")])
        .map_err(|error| {
            format!(
                "the bisect's record is written here but not pushed: {}",
                error.message()
            )
        })?;
    Ok(Ok(verdict))
}

/// The policy at `base` with check `id` alone, a required post check whatever the landing's
/// diff: a bisect asks what each tree does. The manifest digest stays the base's, so the
/// record answers the check there. `Ok(Err(why))` when it cannot answer it.
fn alone(
    git: &Git,
    writer: &Writer,
    id: &CheckId,
    base: &str,
) -> Result<Result<crate::policy::Policy, String>, String> {
    // G91: a landing's post check is judged in the policy at its first parent.
    let Some(policy) = crate::policy::load_at(git, base).map_err(String::from)? else {
        return Ok(Err(format!("{base} has no policy to run the check under")));
    };
    let Some(entry) = policy
        .check(id)
        .filter(|entry| entry.stage == crate::policy::Stage::Post)
    else {
        return Ok(Err(format!(
            "the policy at {base} has no post check {} in {}",
            id.id, id.environment
        )));
    };
    // G163: a record from a producer the check does not list answers nothing.
    if entry
        .producers
        .as_ref()
        .is_some_and(|names| !names.contains(&writer.name))
    {
        return Ok(Err(format!(
            "the check's producers do not list {}, so its runs would answer nothing",
            writer.name
        )));
    }
    let Some(manifest) = policy
        .post
        .iter()
        .find(|post| post.id == id.id && post.environment == id.environment)
        .cloned()
    else {
        return Ok(Err(format!(
            "the post check {} is quarantined at {base}",
            id.id
        )));
    };
    let mut entry = entry.clone();
    entry.when_paths = None;
    let mut only = policy;
    only.checks = vec![entry];
    only.manifest = vec![manifest];
    Ok(Ok(only))
}

fn not_run(red: &Red, why: &str, tested: &[Value]) -> Value {
    json!({
        "check": red.check,
        "environment": red.environment,
        "outcome": "not_run",
        "why": why,
        "tested": tested,
        "runs": tested.len(),
    })
}

fn mark_json(mark: &Mark) -> Value {
    json!({"landing": mark.landing, "change": mark.change})
}

fn tested_json(mark: &Mark, verdict: Verdict) -> Value {
    json!({"landing": mark.landing, "change": mark.change, "verdict": verdict})
}

fn rev(git: &Git, name: &str) -> Result<String, String> {
    git.run(&["rev-parse", "--verify", "-q", name])
        .map(|out| out.trim().to_owned())
        .map_err(|_| format!("{name} is not a commit here"))
}

fn tree_of(git: &Git, commit: &str) -> Result<String, String> {
    rev(git, &format!("{commit}^{{tree}}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bound_is_log2_of_the_range_and_one_rerun() {
        for (landings, runs) in [(2, 2), (3, 3), (4, 3), (5, 4), (8, 4), (9, 5)] {
            assert_eq!(bound(landings), runs, "{landings} landings");
        }
    }
}

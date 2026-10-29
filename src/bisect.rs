//! G244d: culprit finding on a red post check, by first-parent bisection with a rerun at the
//! suspect (the prior art: a trunk-wide test service that finds the breaking change by
//! bisecting the landed range, and runs the suspect again before it names it).
//!
//! A post check (`stage = "post"`, G91) that tests only some trunk tips leaves a [`Suspect`]
//! range: the landings after its newest pass up to its first failure. Off by default, with
//! `[runner] bisect_post = true` in `.bay/config.toml` at trunk, an executor pass narrows it:
//!
//! - Only a red check whose range holds more than one landing and has a pass below it (in the
//!   `[red] window`, or past it down to `[red] reach`, B5), on a
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
/// when `[runner] bisect_post` is off or no such check is red. A fault is reported, never
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

/// The runner's own post checks (`[runner] post`): after its queue's work, each post check of the
/// policy at main's newest tip's first parent runs once on that tip when this box serves its
/// environment and holds no record of it there yet. One run answers every landing since the last
/// run's tip: a pass, or a red the red judgement (`crate::red`) ranges and `[runner] bisect_post`
/// narrows. `None` when off or nothing ran.
#[must_use]
pub fn tips(repo: &Repo) -> Option<Value> {
    if !repo.config.post_tips() {
        return None;
    }
    match tip_runs(repo) {
        Ok(reports) if reports.is_empty() => None,
        Ok(reports) => Some(Value::Array(reports)),
        Err(why) => Some(json!({"error": why})),
    }
}

/// Whether this box serves the environment of at least one of `policy`'s post checks.
fn serves_a_post_check(policy: &crate::policy::Policy, served: &check::Served) -> bool {
    policy
        .checks
        .iter()
        .any(|entry| entry.stage == crate::policy::Stage::Post && served.serves(&entry.environment))
}

fn tip_runs(repo: &Repo) -> Result<Vec<Value>, String> {
    let git = &repo.git;
    let tip = repo.fetch_trunk().map_err(|error| error.message())?;
    let Ok(base) = rev(git, &format!("{tip}^1")) else {
        return Ok(Vec::new());
    };
    let Some(policy) = crate::policy::load_at(git, &base).map_err(String::from)? else {
        return Ok(Vec::new());
    };
    let served = check::Served::read(git);
    // BACKLOG 113 (a fleet finding, 2026-10-28: runners that serve no post check's environment
    // spent 140-166 s of every pass here, fetching evidence and judging main in full, and a new
    // change waited for that pass to end before any runner visited it): a box that can run none
    // of the policy's post checks reads nothing for them.
    if !serves_a_post_check(&policy, &served) {
        return Ok(Vec::new());
    }
    let keys = Keys::load(git);
    repo.fetch_registry();
    repo.fetch_evidence(&keys);
    let registry = Registry::current(git)?;
    let tree = tree_of(git, &tip)?;
    let writer = Writer::of(git, &keys, registry.as_ref())?;
    // A fleet finding (2026-10-27, 0.5.29): with one post check a pass and main moving every few
    // minutes, each new tip began again at the policy's first post check, and the later ones never
    // ran. The checks no landing in the red window answers (`crate::red`'s pending) go first.
    let pending: Vec<(String, String)> = crate::red::judged(git, &tip, registry.as_ref())
        .map(|judged| {
            judged
                .pending
                .into_iter()
                .map(|pending| (pending.check, pending.environment))
                .collect()
        })
        .unwrap_or_default();
    // A fleet finding (2026-10-28: scripts-fast stayed red for three hours after both of its
    // causes were fixed in files off its paths, and every change on its paths waited on it): a
    // red check runs again on each new tip, first, even when nothing on its paths changed, so
    // a fix that landed anywhere can clear it.
    let red: Vec<crate::red::Red> =
        crate::red::red(git, &tip, registry.as_ref()).unwrap_or_default();
    let red_of = |entry: &crate::policy::CheckEntry| {
        red.iter()
            .find(|red| red.check == entry.id && red.environment == entry.environment)
    };
    let is_red = |entry: &crate::policy::CheckEntry| red_of(entry).is_some();
    let mut entries: Vec<&crate::policy::CheckEntry> = policy
        .checks
        .iter()
        .filter(|entry| {
            entry.stage == crate::policy::Stage::Post && served.serves(&entry.environment)
        })
        .collect();
    // G433 (a fleet finding, 2026-10-28): a check that only ever answers `unknown` stays pending,
    // so it went first on every tip, and a check in the policy's tail (edge-py-fast, fifth on its
    // box) ran on no tip for eight hours. The check this box ran longest ago goes first; ties
    // keep the rule above.
    let ran = last_runs(git, &writer, &tip, &entries);
    entries.sort_by_key(|entry| {
        let pending = pending
            .iter()
            .any(|(check, environment)| *check == entry.id && *environment == entry.environment);
        let ago = ran
            .get(&(entry.id.clone(), entry.environment.clone()))
            .copied()
            .unwrap_or(usize::MAX);
        (!is_red(entry), std::cmp::Reverse(ago), !pending)
    });
    let mut reports = Vec::new();
    for entry in entries {
        let id = CheckId::new(&entry.id, &entry.environment);
        if writer.records(git, &tree, &id)? > 0 {
            continue;
        }
        // BACKLOG 117: a check no worker answered here lately waits, and this pass runs the
        // next one.
        if waits_for_a_worker(repo, &id).is_some() {
            continue;
        }
        // G439 (a fleet finding, 2026-10-28): a post run on the tip was asked about the tip's
        // last landing alone, so a check whose adapter picks its tests by changed paths ran
        // nothing ("collected 0 of 1") unless that one landing touched them. It is asked what
        // changed since this box last answered it, and not run when nothing in that range
        // wakes it.
        //
        // G495 (a fleet finding, 2026-10-29: after its red, edge-py-fast answered "collected 0 of 1"
        // on four tips in a row, so it could never pass again): a red check's last record is the
        // previous tip, often a run that collected nothing, so the range was one landing and an
        // adapter that picks its tests by changed paths ran none of the failing ones. A red check
        // is asked what changed since its newest pass before the failure, a range that holds the
        // paths that chose the failing tests; with no pass in the window, the whole span.
        let since = match red_of(entry) {
            Some(red) => red
                .suspect
                .as_ref()
                .and_then(|suspect| suspect.passed.as_ref())
                .map_or_else(
                    || span_start(git, &tip),
                    |passed| Ok(passed.landing.clone()),
                )?,
            None => ran
                .get(&(entry.id.clone(), entry.environment.clone()))
                .map_or_else(
                    || span_start(git, &tip),
                    |ago| rev(git, &format!("{tip}~{ago}")),
                )?,
        };
        if !is_red(entry)
            && policy
                .unwoken(entry, &check::changed_paths(git, &since, &tip)?)
                .is_some()
        {
            continue;
        }
        // One check's fault is that check's, said in its row; the post checks after it still run.
        let verdict = match run_at(
            repo,
            &keys,
            &writer,
            (&id, ("post", json!({"tip": tip}))),
            (&tip, Some(&since)),
        ) {
            Ok(Ok(verdict)) => json!(verdict),
            Ok(Err(why)) | Err(why) => json!({"not_run": why}),
        };
        reports.push(json!({"check": entry.id, "environment": entry.environment, "tip": tip, "verdict": verdict}));
        // One post check a pass: the next pass runs the next (see `executor::pass`).
        break;
    }
    Ok(reports)
}

/// How many landings back from `tip` this box last recorded each of `entries` (G433), over the
/// newest [`RAN_SPAN`] commits of main's first-parent line; a check it ran in none is absent.
/// Only an order: a line or a note it cannot read counts as no record, never as no post work.
fn last_runs(
    git: &Git,
    writer: &Writer,
    tip: &str,
    entries: &[&crate::policy::CheckEntry],
) -> std::collections::HashMap<(String, String), usize> {
    let span = RAN_SPAN.to_string();
    let mut ran = std::collections::HashMap::new();
    let Ok(trees) = git.run(&["log", "--first-parent", "-n", &span, "--format=%T", tip]) else {
        return ran;
    };
    for (ago, tree) in trees.lines().enumerate() {
        if ran.len() == entries.len() {
            break;
        }
        for record in evidence::producer_records(git, &writer.name, tree).unwrap_or_default() {
            if record.kind == evidence::KIND_CHECK
                && entries
                    .iter()
                    .any(|entry| record.check == CheckId::new(&entry.id, &entry.environment))
            {
                ran.entry((record.check.id, record.check.environment))
                    .or_insert(ago);
            }
        }
    }
    ran
}

/// How far back main's line [`last_runs`] reads: a check this box ran on none of these many
/// landings goes before every one it ran on.
const RAN_SPAN: usize = 64;

/// G439: the oldest commit of main's newest [`RAN_SPAN`] first-parent commits from `tip`, the
/// start of the range a post check that this box never answered there is asked about.
fn span_start(git: &Git, tip: &str) -> Result<String, String> {
    let span = RAN_SPAN.to_string();
    git.run(&["rev-list", "--first-parent", "--max-count", &span, tip])
        .map_err(|error| error.message())?
        .lines()
        .last()
        .map(str::to_owned)
        .ok_or_else(|| format!("{tip} has no history"))
}

/// Items 1 and 2: the runner warms results on main's tip (`[runner] warm`). After its queue's
/// work, each pre check with `footprint = "observed"` in the policy at main's newest tip runs
/// once on that tip as a candidate of no change (the tip, on the tip, under its own policy),
/// on a box that serves its environment, when this box holds no record of it there yet. A
/// later candidate on that tip carries the record for every check whose observed reads its
/// change does not touch, so a change runs only the checks it reaches. `None` when off or
/// nothing ran.
#[must_use]
pub fn warm(repo: &Repo) -> Option<Value> {
    if !repo.config.warm() {
        return None;
    }
    match warm_runs(repo) {
        Ok(reports) if reports.is_empty() => None,
        Ok(reports) => Some(Value::Array(reports)),
        Err(why) => Some(json!({"error": why})),
    }
}

fn warm_runs(repo: &Repo) -> Result<Vec<Value>, String> {
    let git = &repo.git;
    let keys = Keys::load(git);
    repo.fetch_registry();
    repo.fetch_evidence(&keys);
    let registry = Registry::current(git)?;
    let tip = repo.fetch_trunk().map_err(|error| error.message())?;
    let Some(policy) = crate::policy::load_at(git, &tip).map_err(String::from)? else {
        return Ok(Vec::new());
    };
    let served = check::Served::read(git);
    let tree = tree_of(git, &tip)?;
    let writer = Writer::of(git, &keys, registry.as_ref())?;
    let mut reports = Vec::new();
    for entry in policy.checks.iter().filter(|entry| {
        entry.stage == crate::policy::Stage::Pre
            && entry.footprint == crate::policy::FootprintMode::Observed
            && served.serves(&entry.environment)
    }) {
        let id = CheckId::new(&entry.id, &entry.environment);
        if writer.records(git, &tree, &id)? > 0 {
            continue;
        }
        let verdict = match run_at(
            repo,
            &keys,
            &writer,
            (&id, ("warm", json!({"tip": tip}))),
            (&tip, None),
        )? {
            Ok(verdict) => json!(verdict),
            Err(why) => json!({"not_run": why}),
        };
        reports.push(json!({"check": entry.id, "environment": entry.environment, "tip": tip, "verdict": verdict}));
    }
    Ok(reports)
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
    let trunk = repo.fetch_trunk().map_err(|error| error.message())?;
    let served = check::Served::read(git);
    // BACKLOG 113: a box that can run none of the policy's post checks finds no culprit, so it
    // judges nothing for them.
    let post_work = match crate::policy::load_at(git, &trunk) {
        Ok(Some(policy)) => serves_a_post_check(&policy, &served),
        // An invalid policy has no post check to bisect, as the red judgement reads it.
        Ok(None) | Err(crate::policy::LoadError::Invalid(_)) => false,
        Err(crate::policy::LoadError::Git(why)) => return Err(why),
    };
    if !post_work {
        return Ok(Vec::new());
    }
    let keys = Keys::load(git);
    repo.fetch_registry();
    repo.fetch_evidence(&keys);
    let registry = Registry::current(git)?;
    let judgement = crate::red::judged(git, &trunk, registry.as_ref())?;
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
            let why = format!(
                "no merge of the newest {} on main passes the check, so the range has no good end",
                suspect.searched.unwrap_or_default()
            );
            reports.push(not_run(red, &why, &[]));
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
        let writer = Writer::of(&repo.git, &keys, registry.as_ref())?;
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
    fn of(git: &Git, keys: &Keys, registry: Option<&Registry>) -> Result<Self, String> {
        let signer = match (Registry::signing(registry), &keys.producer) {
            (None, _) => {
                // G297: unsigned evidence goes under this clone's own name.
                return Ok(Self {
                    name: crate::signing::own_producer_name(git),
                    signer: None,
                });
            }
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
    let window = json!({
        "last_passed": suspect.passed.as_ref().map(|mark| mark.landing.clone()),
        "first_failed": suspect.failed.landing,
    });
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
        let verdict = match run_at(
            repo,
            keys,
            writer,
            (&id, ("bisect", window.clone())),
            (&mark.landing, None),
        )? {
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
            let verdict = match run_at(
                repo,
                keys,
                writer,
                (&id, ("bisect", window.clone())),
                (&mark.landing, None),
            )? {
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
/// How long a check that no worker answered on this box waits before this box tries it again
/// (BACKLOG 117, a fleet finding, 2026-10-28: the one post runner waited 383-400 s on each of
/// eight venue6 tickets that never started, and wrote no record, so the same check came first
/// in every pass and its other post checks were silent for two hours). A record of the check
/// ends the wait.
const UNAVAILABLE_WAIT: std::time::Duration = std::time::Duration::from_hours(1);

/// [`UNAVAILABLE_WAIT`]; a test build honours `BAY_TEST_UNAVAILABLE_WAIT_S`, a release build
/// never does.
fn unavailable_wait() -> std::time::Duration {
    if option_env!("BAY_RELEASE_VERSION").is_none()
        && let Some(secs) = std::env::var("BAY_TEST_UNAVAILABLE_WAIT_S")
            .ok()
            .and_then(|secs| secs.parse().ok())
    {
        return std::time::Duration::from_secs(secs);
    }
    UNAVAILABLE_WAIT
}

/// The file where this box notes when no worker answered each check: `<check>@<environment>`
/// to the Unix second.
fn unavailable_file(repo: &Repo) -> std::path::PathBuf {
    repo.common_dir.join("bay").join("post-unavailable.json")
}

fn unavailable_key(id: &CheckId) -> String {
    format!("{}@{}", id.id, id.environment)
}

fn unavailable_notes(repo: &Repo) -> std::collections::BTreeMap<String, u64> {
    std::fs::read_to_string(unavailable_file(repo))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Records that no worker answered `id` at `now`, or, with `None`, that it has a record again.
fn note_unavailable(repo: &Repo, id: &CheckId, now: Option<u64>) {
    let mut notes = unavailable_notes(repo);
    let key = unavailable_key(id);
    let changed = match now {
        Some(now) => notes.insert(key, now) != Some(now),
        None => notes.remove(&key).is_some(),
    };
    if !changed {
        return;
    }
    let path = unavailable_file(repo);
    let tmp = path.with_extension("tmp");
    if let Ok(text) = serde_json::to_string(&notes)
        && std::fs::write(&tmp, text).is_ok()
    {
        let _ = crate::durable::rename(&tmp, &path);
    }
}

/// The seconds `id` still waits, when no worker answered it here within [`unavailable_wait`].
fn waits_for_a_worker(repo: &Repo, id: &CheckId) -> Option<u64> {
    let at = *unavailable_notes(repo).get(&unavailable_key(id))?;
    let left = at
        .saturating_add(unavailable_wait().as_secs())
        .saturating_sub(crate::live::unix_now());
    (left > 0).then_some(left)
}

fn run_at(
    repo: &Repo,
    keys: &Keys,
    writer: &Writer,
    (id, why): (&CheckId, (&str, Value)),
    (commit, since): (&str, Option<&str>),
) -> Result<Result<Verdict, String>, String> {
    if let Some(left) = waits_for_a_worker(repo, id) {
        return Ok(Err(format!(
            "no worker answered the check here lately; it is tried again in {left} s"
        )));
    }
    let git = &repo.git;
    let (field, body) = why;
    // A warm run is a candidate of no change on the tip, judged as one on that tip: its base
    // is the tip itself. A post run and a bisect judge a landing under its first parent.
    let warm = field == "warm";
    let base = if warm {
        commit.to_owned()
    } else {
        rev(git, &format!("{commit}^1"))?
    };
    let tree = tree_of(git, commit)?;
    let base_tree = tree_of(git, &base)?;
    let stage = if warm {
        crate::policy::Stage::Pre
    } else {
        crate::policy::Stage::Post
    };
    let only = match alone(git, writer, id, &base, stage)? {
        Ok(only) => only,
        Err(why) => return Ok(Err(why)),
    };
    // A post run on the tip names what changed since `since` (G439); a bisect step names its
    // landing's own change, which is what it judges.
    let changed = check::changed_paths(git, since.unwrap_or(&base), commit)?;
    let run = writer.records(git, &tree, id)? + 1;
    let short = commit.get(..12).unwrap_or(commit);
    // A fleet finding (2026-10-27): the id named no check, so the second post check on a tip
    // took the first one's evidence id, its record was refused as a second record under one id,
    // and every later post check of that tip never ran: four of five were silent for 22 hours.
    let check: String = id
        .id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let candidate_id = format!("{field}-{check}-{short}-{run}");
    let scratch = check::scratch_root(&repo.common_dir)?.join(&candidate_id);
    let _cleanup = crate::run::Scratch(scratch.clone());
    let mut keep = |record: &mut EvidenceRecord| -> Result<(), String> {
        // Post evidence, as a post producer writes it: no pre-publication provenance. A warm
        // record is pre-publication evidence of the tip, and keeps it, so a candidate carries it.
        if !warm {
            record.policy_provenance = None;
        }
        record.unknown.insert(field.to_owned(), body.clone());
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
            turn: None,
        },
        &writer.name,
        &check::Limits {
            budget: None,
            finished: Vec::new(),
            heartbeat: None,
            hidden: check::hidden_for(repo, keys),
            sandbox: check::Sandbox::here().reading(git),
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
    let _ = check::remove_tree(&scratch);
    answered(repo, writer, id, ran?)
}

/// What `ran` answered for `id`: its verdict, its record pushed; or why it gave none. A check
/// no worker answers is noted, so this box waits before it tries it again (BACKLOG 117).
fn answered(
    repo: &Repo,
    writer: &Writer,
    id: &CheckId,
    ran: check::Ran,
) -> Result<Result<Verdict, String>, String> {
    if let Some((_, reason)) = ran.unavailable {
        note_unavailable(repo, id, Some(crate::live::unix_now()));
        return Ok(Err(format!("no worker answers the check: {reason}")));
    }
    let Some(verdict) = ran.records.first().map(|record| record.verdict) else {
        return Ok(Err("the check wrote no record".to_owned()));
    };
    note_unavailable(repo, id, None);
    let notes = evidence::notes_ref(&writer.name);
    repo.git
        .run(&["push", "-q", repo.remote(), &format!("{notes}:{notes}")])
        .map_err(|error| {
            format!(
                "the post check's record is written here but not pushed: {}",
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
    stage: crate::policy::Stage,
) -> Result<Result<crate::policy::Policy, String>, String> {
    // G91: a landing's post check is judged in the policy at its first parent.
    let Some(policy) = crate::policy::load_at(git, base).map_err(String::from)? else {
        return Ok(Err(format!("{base} has no policy to run the check under")));
    };
    let Some(entry) = policy.check(id).filter(|entry| entry.stage == stage) else {
        return Ok(Err(format!(
            "the policy at {base} has no {stage:?} check {} in {}",
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
    let listed = if stage == crate::policy::Stage::Post {
        &policy.post
    } else {
        &policy.manifest
    };
    let Some(manifest) = listed
        .iter()
        .find(|post| post.id == id.id && post.environment == id.environment)
        .cloned()
    else {
        return Ok(Err(format!("the check {} is quarantined at {base}", id.id)));
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

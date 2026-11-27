//! G244d: culprit finding on a red post check, by first-parent bisection with a rerun at the
//! suspect (the prior art: a trunk-wide test service that finds the breaking change by
//! bisecting the landed range, and runs the suspect again before it names it).
//!
//! A post check (`stage = "post"`, G91) that tests only some trunk tips leaves a [`Suspect`]
//! range: the landings after its newest pass up to its first failure. Off by default, with
//! `[runner] bisect_post = true` in `.bay/config.toml` at trunk, an executor pass narrows it:
//!
//! - Only a red check whose range holds more than one landing, with a pass below it (in the
//!   `[red] window`, or past it down to `[red] reach`, B5) or a base not run yet (BACKLOG 348),
//!   on a box that serves the check's environment (`bay.checkEnvironment`), and only when the
//!   check's `producers` list, if any, names this box's producer.
//! - BACKLOG 348 (a load-run finding: a post check failed on its first runs, after the merge that
//!   broke it, so its range had no good end, and no culprit or undo came): a range with no pass
//!   below it is run once on its base, its oldest landing (the one that added the check, or the
//!   oldest read), as a change of nothing on it under that landing's own policy. A pass there is
//!   the range's good end, and the next pass bisects above it. A failure there makes the red the
//!   check's own (`crate::red`): no merge is bisected or named.
//! - G627 (fleet findings, 2026-11-02: a canary merge and a card merge were each named the
//!   culprit of a flaky test on one failing record): a range of one landing whose culprit stands
//!   on fewer than two records of the check in its tree takes the rerun at the suspect too, so
//!   a flaky first failure is found before it holds the line.
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
//! - G857: of the live runners that serve the check's environment, one bisects a red: the one
//!   that runs the check on main's tip (G822). Another bisects it once that one's presence reads
//!   stale, or no producer answered the check in the range for twice its timeout.
//!
//! Nothing is stored but the evidence: the culprit is judged from the records on each read.
//!
//! #123: the runner's post work is planned here ([`plan`]) and run elsewhere (`crate::executor`)
//! as [`Piece`]s, one environment each: a post check on main's tip, the warm runs, or the
//! bisects of red post checks. A pass plans at most one piece for each environment this box
//! serves and no running piece holds, so the post work of different environments runs side by
//! side, and the work of one environment runs one piece at a time.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::check::{self, Candidate};
use crate::evidence::{self, CheckId, EvidenceRecord, Verdict};
use crate::git::Git;
use crate::red::{Mark, Red, Suspect};
use crate::registry::Registry;
use crate::repo::Repo;
use crate::signing::{Keys, Signer};

/// A bisect's outcome when the range narrowed to one landing that failed twice.
pub const CULPRIT: &str = "culprit";

/// The post work one pass plans ([`plan`]).
pub struct Plan {
    /// What needs no run, by kind (`post`, `warm` or `bisect`): a stage's fault as
    /// `{"error": why}`, or the bisects that cannot run here.
    pub rows: Vec<(&'static str, Value)>,
    /// The pieces to run, at most one for each environment.
    pub pieces: Vec<Piece>,
}

/// One piece of post work, in one environment, run on its own thread ([`Piece::run`]).
pub struct Piece {
    /// The environment the piece runs checks of.
    pub environment: String,
    /// `post`, `warm` or `bisect`: the kind of its rows.
    pub kind: &'static str,
    job: Job,
    keys: Keys,
    writer: Writer,
}

/// One post check on main's tip, asked what changed since `since`, and its `rerun` units.
struct TipRun {
    id: CheckId,
    tip: String,
    since: String,
    post: Value,
    rerun: Vec<String>,
}

enum Job {
    /// A post check on main's tip.
    Tip(Box<TipRun>),
    /// Each of `ids`, an observed pre check, as a candidate of no change on `tip`.
    Warm { tip: String, ids: Vec<CheckId> },
    /// The bisect of each red post check.
    Bisect(Vec<Red>),
}

impl Piece {
    /// Runs the piece: an array of what each of its runs did, or `{"error": why}` for a fault
    /// that ends it. A fault is reported, never fatal to the runner.
    #[must_use]
    pub fn run(self, repo: &Repo) -> Value {
        let Self {
            environment,
            job,
            keys,
            writer,
            ..
        } = self;
        let mut reports = Vec::new();
        match job {
            Job::Tip(run) => {
                let TipRun {
                    id,
                    tip,
                    since,
                    post,
                    rerun,
                } = *run;
                // One check's fault is that check's, said in its row.
                let verdict = match run_at(
                    repo,
                    &keys,
                    &writer,
                    (&id, ("post", post)),
                    (&tip, Some(&since), &rerun),
                    Under::FirstParent,
                ) {
                    Ok(Ok(answer)) => answer.word(),
                    Ok(Err(why)) | Err(why) => json!({"not_run": why}),
                };
                reports.push(json!({"check": id.id, "environment": environment, "tip": tip, "verdict": verdict}));
            }
            Job::Warm { tip, ids } => {
                for id in ids {
                    let verdict = match run_at(
                        repo,
                        &keys,
                        &writer,
                        (&id, ("warm", json!({"tip": tip}))),
                        (&tip, None, &[]),
                        Under::Itself,
                    ) {
                        Ok(Ok(answer)) => answer.word(),
                        Ok(Err(why)) => json!({"not_run": why}),
                        Err(why) => return json!({"error": why}),
                    };
                    reports.push(json!({"check": id.id, "environment": id.environment, "tip": tip, "verdict": verdict}));
                }
            }
            Job::Bisect(reds) => {
                for red in &reds {
                    let Some(suspect) = &red.suspect else {
                        continue;
                    };
                    match bisect(repo, &keys, &writer, red, suspect) {
                        Ok(report) => reports.push(report),
                        Err(why) => return json!({"error": why}),
                    }
                }
            }
        }
        Value::Array(reports)
    }
}

/// What one stage plans: its rows that need no run, and its pieces.
type Planned = (Vec<Value>, Vec<Piece>);

/// One stage of the post work, given the environments it may not plan a piece in.
type Stage = fn(&Repo, &BTreeSet<String>, &mut Fresh) -> Result<Planned, String>;

/// G860 (a fleet measurement, 2026-11-22: a runner's post pass fetched main twice, the registry
/// and the evidence notes once a stage, milliseconds apart): what a plan read from the remote,
/// read once for all its stages. Each stage reads main, the registry and the notes as the first
/// stage that needed them fetched them.
#[derive(Default)]
struct Fresh {
    tip: Option<String>,
    registry: bool,
    evidence: bool,
    presences: bool,
    /// G932: the presences that were members when the plan's fetch brought them ([`Fresh::live`]).
    live: Option<Vec<Value>>,
}

impl Fresh {
    /// Main's tip on the remote: fetched by the plan's first stage that reads it.
    fn tip(&mut self, repo: &Repo) -> Result<String, String> {
        if let Some(tip) = &self.tip {
            return Ok(tip.clone());
        }
        let tip = repo.fetch_trunk().map_err(|error| error.message())?;
        self.tip = Some(tip.clone());
        Ok(tip)
    }

    /// The registry, fetched once for the plan.
    fn registry(&mut self, repo: &Repo) {
        if !std::mem::replace(&mut self.registry, true) {
            repo.fetch_registry();
        }
    }

    /// Every other writer's evidence notes, fetched once for the plan, and with `presences` the
    /// runners' presences in the same fetch (G822).
    fn evidence(&mut self, repo: &Repo, keys: &Keys, presences: bool) {
        if self.evidence && (self.presences || !presences) {
            return;
        }
        let more = if presences {
            vec![crate::presence::refspec()]
        } else {
            Vec::new()
        };
        repo.fetch_evidence_with(keys, &more);
        self.evidence = true;
        self.presences |= presences;
        if presences {
            self.live = Some(crate::presence::members(&repo.git, repo.config.lease()));
        }
    }

    /// G932 (a fleet finding, 2026-11-27: a runner ran a post check of 640-700 s on every tip its
    /// owner ran it on too): the runners' presences that read live, judged as the plan's fetch
    /// brought them. A stage judges its checks up to a minute after that
    /// fetch, by when every peer's last beat in it read stale, so the runner elected itself to
    /// every check its peers owned. A plan that fetched no presences reads the store now. G940:
    /// a peer counts while it is a member ([`crate::presence::members`]), for its lease, since
    /// its presence can reach this store a minute late.
    fn live(&self, repo: &Repo) -> Vec<Value> {
        self.live
            .clone()
            .unwrap_or_else(|| crate::presence::members(&repo.git, repo.config.lease()))
    }
}

/// #123: the post work this pass starts, after its queue's work, in the environments none of
/// `busy` (the environments of the pieces still running) names. Its stages, in order: the
/// runner's own post checks on main's tip ([`tips`]), the warm runs ([`warm`]), and the culprit
/// of a red post check ([`culprits`]); a stage plans no piece in an environment an earlier one
/// took. A stage's fault is its row, and the stages after it wait for the next pass.
#[must_use]
pub fn plan(repo: &Repo, busy: &BTreeSet<String>) -> Plan {
    let stages: [(&'static str, Stage); 3] = [("post", tips), ("warm", warm), ("bisect", culprits)];
    let mut plan = Plan {
        rows: Vec::new(),
        pieces: Vec::new(),
    };
    let mut fresh = Fresh::default();
    for (kind, stage) in stages {
        let mut taken = busy.clone();
        taken.extend(plan.pieces.iter().map(|piece| piece.environment.clone()));
        match stage(repo, &taken, &mut fresh) {
            Ok((rows, pieces)) => {
                if !rows.is_empty() {
                    plan.rows.push((kind, Value::Array(rows)));
                }
                plan.pieces.extend(pieces);
            }
            Err(why) => {
                plan.rows.push((kind, json!({"error": why})));
                break;
            }
        }
    }
    plan
}

/// Whether this box serves the environment of at least one of `policy`'s post checks that none
/// of `taken` names.
fn serves_a_post_check(
    policy: &crate::policy::Policy,
    served: &check::Served,
    taken: &BTreeSet<String>,
) -> bool {
    !post_checks(policy, served, taken).is_empty()
}

/// `policy`'s post checks whose environment this box serves and none of `taken` names.
fn post_checks<'a>(
    policy: &'a crate::policy::Policy,
    served: &check::Served,
    taken: &BTreeSet<String>,
) -> Vec<&'a crate::policy::CheckEntry> {
    policy
        .checks
        .iter()
        .filter(|entry| {
            entry.stage == crate::policy::Stage::Post
                && served.serves(&entry.environment)
                && !taken.contains(&entry.environment)
        })
        .collect()
}

/// BACKLOG 113 (G444): whether this box serves the environment of at least one post check of
/// the policy at `commit`. A box that serves none judges nothing for post work.
pub(crate) fn serves_post_work(git: &Git, commit: &str) -> bool {
    match crate::policy::load_at(git, commit) {
        Ok(Some(policy)) => {
            serves_a_post_check(&policy, &check::Served::read(git), &BTreeSet::new())
        }
        Ok(None) | Err(_) => false,
    }
}

/// The runner's own post checks (`[runner] post`): after its queue's work, each post check of the
/// policy at main's newest tip's first parent runs once on that tip when this box serves its
/// environment, holds no record of it there yet, no other producer answered it there
/// ([`answered_elsewhere`], G625), and no other live runner runs it ([`left_to`], G822). One run
/// answers every landing since the last run's tip: a pass,
/// or a red the red judgement (`crate::red`) ranges and `[runner] bisect_post` narrows. A red check
/// runs again on each new tip (G492), and its request names `rerun`: the units that failed in its
/// newest failing record (G540). One piece for each environment none of `taken` names: its first
/// check in the order [`in_turn`] gives.
fn tips(repo: &Repo, taken: &BTreeSet<String>, fresh: &mut Fresh) -> Result<Planned, String> {
    if !repo.config.post_tips() {
        return Ok((Vec::new(), Vec::new()));
    }
    let git = &repo.git;
    let tip = fresh.tip(repo)?;
    let Ok(base) = rev(git, &format!("{tip}^1")) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let Some(policy) = crate::policy::load_at(git, &base).map_err(String::from)? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let served = check::Served::read(git);
    // BACKLOG 113 (a fleet finding, 2026-10-28: runners that serve no post check's environment
    // spent 140-166 s of every pass here, fetching evidence and judging main in full, and a new
    // change waited for that pass to end before any runner visited it): a box that can run none
    // of the policy's post checks, or whose every such environment runs a piece, reads nothing.
    let entries = post_checks(&policy, &served, taken);
    if entries.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let keys = Keys::load(git);
    fresh.registry(repo);
    fresh.evidence(repo, &keys, true);
    let registry = Registry::current(git)?;
    let tree = tree_of(git, &tip)?;
    let writer = Writer::of(git, &keys, registry.as_ref())?;
    // A fleet finding (2026-10-28: a scripts check stayed red for three hours after both of its
    // causes were fixed in files off its paths, and every change on its paths waited on it): a
    // red check runs again on each new tip, first (after a check starved for half the red
    // window, G582), even when nothing on its paths changed, so a fix that landed anywhere can
    // clear it.
    //
    // E39 (a fleet finding, 2026-10-29: once a fix named a Python check's owner, its next post runs
    // asked about one landing alone and named no failed unit, so five known failing units went
    // unrun): a check whose fix waits for its run (`pending_fix`) is asked what a red check is
    // asked, so the run that answers the fix runs the failing units.
    let reds = crate::red::holding(git, &tip, registry.as_ref()).unwrap_or_default();
    let red_of = |entry: &crate::policy::CheckEntry| {
        reds.iter()
            .find(|red| red.check == entry.id && red.environment == entry.environment)
    };
    let trust = (registry.as_ref(), &writer, &policy);
    let (entries, ran) = in_turn(git, trust, &tip, entries, &reds);
    let here = (tip.as_str(), evidence::records_for(git, &tree)?);
    let elsewhere = answered_elsewhere(git, trust, &entries, here)?;
    let live = fresh.live(repo);
    let now = crate::presence::reader_now();
    let landed = git
        .run(&["log", "-1", "--format=%ct", &tip])
        .ok()
        .and_then(|time| time.trim().parse::<u64>().ok());
    let mut pieces: Vec<Piece> = Vec::new();
    for entry in entries {
        if pieces
            .iter()
            .any(|piece| piece.environment == entry.environment)
        {
            continue;
        }
        let id = CheckId::new(&entry.id, &entry.environment);
        if writer.records(git, &tree, &id)? > 0 || elsewhere.contains(&id) {
            continue;
        }
        let last = ran.get(&(entry.id.clone(), entry.environment.clone()));
        if left_to(entry, (&keys.executor_name, &live), last, (landed, now)).is_some() {
            continue;
        }
        // BACKLOG 117: a check no worker answered here lately waits, and this pass runs the
        // next one; a red check waits less (G573).
        if waits_for_a_worker(repo, &id, red_of(entry).is_some()).is_some() {
            continue;
        }
        // G439 (a fleet finding, 2026-10-28): a post run on the tip was asked about the tip's
        // last landing alone, so a check whose adapter picks its tests by changed paths ran
        // nothing ("collected 0 of 1") unless that one landing touched them. It is asked what
        // changed since this box last answered it, and not run when nothing in that range
        // wakes it.
        //
        // G495 (a fleet finding, 2026-10-29: once red, a Python check answered "collected 0 of 1"
        // on four tips in a row, so it could never pass again): a red check's last record is the
        // previous tip, often a run that collected nothing, so the range was one landing and an
        // adapter that picks its tests by changed paths ran none of the failing ones. A red check
        // is asked what changed since its newest pass before the failure, a range that holds the
        // paths that chose the failing tests; with no pass in the window, the whole span.
        //
        // G629 (a fleet finding, 2026-11-02: after its red, a Python check ran 2,073 units on each
        // new tip, in 627-645 s, because the culprit's paths stayed in every tip's range and
        // picked a whole service): a red check whose newest failing run was asked its failed
        // units again and answered each one is asked what changed since that run's landing.
        // That run already tested the range before it; this one runs the failed units
        // ([`post_request`]) and what the landings after it wake.
        let failing = match red_of(entry) {
            Some(red) => failing(git, registry.as_ref(), red, &id)?,
            None => Failing::default(),
        };
        let since = match red_of(entry) {
            Some(red) => match (&red.suspect, failing.answered_rerun) {
                (Some(suspect), true) => suspect.newest_failed.landing.clone(),
                (suspect, _) => suspect
                    .as_ref()
                    .and_then(|suspect| suspect.passed.as_ref())
                    .map_or_else(
                        || span_start(git, &tip),
                        |passed| Ok(passed.landing.clone()),
                    )?,
            },
            None => last.map_or_else(
                || span_start(git, &tip),
                |last| rev(git, &format!("{tip}~{}", last.ago)),
            )?,
        };
        if red_of(entry).is_none()
            && policy
                .unwoken(entry, &check::changed_paths(git, &since, &tip)?)
                .is_some()
        {
            continue;
        }
        let (post, rerun) = post_request(failing.units, &tip);
        // One post check an environment a pass: the next pass plans the next (#123).
        pieces.push(Piece {
            environment: entry.environment.clone(),
            kind: "post",
            job: Job::Tip(Box::new(TipRun {
                id,
                tip: tip.clone(),
                since,
                post,
                rerun,
            })),
            keys: keys.clone(),
            writer: writer.clone(),
        });
    }
    Ok((Vec::new(), pieces))
}

/// `entries` in the order their post runs come in, and how many landings back this box last
/// recorded each, or another producer answered it ([`last_runs`]): a check no box has answered
/// for half the red window first (G582), then a red check (G492), then the one answered longest
/// ago (G433), then the ones no landing in the red window answers (`crate::red`'s pending).
fn in_turn<'a>(
    git: &Git,
    (registry, writer, policy): (Option<&Registry>, &Writer, &crate::policy::Policy),
    tip: &str,
    mut entries: Vec<&'a crate::policy::CheckEntry>,
    reds: &[Red],
) -> (
    Vec<&'a crate::policy::CheckEntry>,
    std::collections::HashMap<(String, String), LastRun>,
) {
    // A fleet finding (2026-10-27, 0.5.29): with one post check a pass and main moving every few
    // minutes, each new tip began again at the policy's first post check, and the later ones never
    // ran. The checks no landing in the red window answers (`crate::red`'s pending) go first.
    let pending: Vec<(String, String)> = crate::red::judged(git, tip, registry)
        .map(|judged| {
            judged
                .pending
                .into_iter()
                .map(|pending| (pending.check, pending.environment))
                .collect()
        })
        .unwrap_or_default();
    // G433 (a fleet finding, 2026-10-28): a check that only ever answers `unknown` stays pending,
    // so it went first on every tip, and a check in the policy's tail (a Python check, fifth on its
    // box) ran on no tip for eight hours. The check this box ran longest ago goes first; ties
    // keep the rule above.
    let ran = last_runs(git, (registry, writer, policy), tip, &entries);
    // G582 (a fleet finding, 2026-10-30: with one runner for its environment and red Python check
    // runs of ~30 min, a scripts check ran on no tip for 49 merges and passed the red window, so a
    // failure there could no longer be ranged): a red check goes first on each new tip, and one
    // piece an environment a pass, so the other checks of its environment starved. A check this
    // box has not answered for half the window goes before a red one, while a run still ranges
    // its gap. Only an order: a window bay cannot read counts as the default.
    let starved_at =
        (crate::red::window(git, tip).unwrap_or(crate::red::DEFAULT_WINDOW) / 2).max(1);
    entries.sort_by_key(|entry| {
        let is_red = reds
            .iter()
            .any(|red| red.check == entry.id && red.environment == entry.environment);
        let pending = pending
            .iter()
            .any(|(check, environment)| *check == entry.id && *environment == entry.environment);
        let ago = ran
            .get(&(entry.id.clone(), entry.environment.clone()))
            .map_or(usize::MAX, |last| last.ago);
        let starved = !is_red && ago >= starved_at;
        (!starved, !is_red, std::cmp::Reverse(ago), !pending)
    });
    (entries, ran)
}

/// The body of a post run on `tip` and the units it asks to run again: none, or, for a red
/// check, the units that failed in the red's newest failing record ([`failing`]). The body names
/// them too, so the record says what its run was asked.
fn post_request(rerun: Vec<String>, tip: &str) -> (Value, Vec<String>) {
    let mut body = json!({"tip": tip});
    if !rerun.is_empty() {
        body["rerun"] = json!(rerun);
    }
    (body, rerun)
}

/// What a red check's newest failing record says about its next run ([`failing`]).
#[derive(Default)]
struct Failing {
    /// The units that failed, sorted, once each.
    units: Vec<String>,
    /// G629: the record's own run was asked to run units again (`post.rerun`), and the record
    /// answers each of them, as a unit or in `gone`.
    answered_rerun: bool,
}

/// G540 (a fleet finding, 2026-10-29: a scripts check failed on 3 units, and every later post run
/// asked its adapter only about the files changed since, which no test read, so the red stood):
/// the units that failed in `red`'s newest failing record of `id`, read from the records the red
/// judgement counts at its newest failing landing, and whether that record answered the units
/// its own run was asked again (G629). Empty when that record names no failing unit.
fn failing(
    git: &Git,
    registry: Option<&Registry>,
    red: &Red,
    id: &CheckId,
) -> Result<Failing, String> {
    let Some(suspect) = &red.suspect else {
        return Ok(Failing::default());
    };
    let tree = tree_of(git, &suspect.newest_failed.landing)?;
    let records = evidence::counted(git, evidence::records_for(git, &tree)?, registry)?;
    let Some(newest) = records
        .iter()
        .filter(|record| {
            record.kind == evidence::KIND_CHECK
                && record.check == *id
                && record.verdict == Verdict::ProductFailure
        })
        .max_by(|one, other| one.at.cmp(&other.at))
    else {
        return Ok(Failing::default());
    };
    let mut units: Vec<String> = newest
        .units
        .iter()
        .filter(|unit| unit.verdict == Verdict::ProductFailure)
        .map(|unit| unit.id.clone())
        .collect();
    units.sort();
    units.dedup();
    let asked = strings(
        newest
            .unknown
            .get("post")
            .and_then(|post| post.get("rerun")),
    );
    let gone = strings(newest.unknown.get(check::GONE));
    let answered_rerun = !units.is_empty()
        && !asked.is_empty()
        && asked
            .iter()
            .all(|unit| gone.contains(unit) || newest.units.iter().any(|ran| ran.id == *unit));
    Ok(Failing {
        units,
        answered_rerun,
    })
}

/// G815: the units that failed in `red`'s newest failing record ([`failing`]): what a run of
/// its check on a candidate before the merge asks again.
///
/// # Errors
///
/// As [`failing`].
pub(crate) fn red_units(
    git: &Git,
    registry: Option<&Registry>,
    red: &Red,
) -> Result<(CheckId, Vec<String>), String> {
    let id = CheckId::new(&red.check, &red.environment);
    let units = failing(git, registry, red, &id)?.units;
    Ok((id, units))
}

/// The strings of a JSON list; none for anything else.
fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// How many landings back from `tip` this box last recorded each of `entries` (G433), or another
/// producer answered it there ([`answered_elsewhere`], G625), over the newest [`RAN_SPAN`]
/// commits of main's first-parent line; a check answered on none is absent. One read for every
/// landing's records (G196). Only an order: a line or a note it cannot read counts as no record,
/// never as no post work.
fn last_runs(
    git: &Git,
    trust: (Option<&Registry>, &Writer, &crate::policy::Policy),
    tip: &str,
    entries: &[&crate::policy::CheckEntry],
) -> std::collections::HashMap<(String, String), LastRun> {
    let span = RAN_SPAN.to_string();
    let mut ran = std::collections::HashMap::new();
    let Ok(line) = git.run(&["log", "--first-parent", "-n", &span, "--format=%H %T", tip]) else {
        return ran;
    };
    let landings: Vec<(&str, &str)> = line.lines().filter_map(|l| l.split_once(' ')).collect();
    let trees: Vec<&str> = landings.iter().map(|(_, tree)| *tree).collect();
    let Ok(records) = evidence::records_for_many(git, &trees) else {
        return ran;
    };
    for (ago, ((commit, _), records)) in landings.iter().zip(records).enumerate() {
        if ran.len() == entries.len() {
            break;
        }
        let own: Vec<CheckId> = records
            .iter()
            .filter(|record| {
                record.producer.name == trust.1.name && record.kind == evidence::KIND_CHECK
            })
            .map(|record| record.check.clone())
            .collect();
        let times: Vec<(CheckId, u64)> = records
            .iter()
            .filter(|record| record.kind == evidence::KIND_CHECK)
            .filter_map(|record| Some((record.check.clone(), crate::clock::unix_of(&record.at)?)))
            .collect();
        let elsewhere =
            answered_elsewhere(git, trust, entries, (*commit, records)).unwrap_or_default();
        for check in own.into_iter().chain(elsewhere) {
            if entries
                .iter()
                .any(|entry| check == CheckId::new(&entry.id, &entry.environment))
            {
                let at = times
                    .iter()
                    .filter(|(answered, _)| *answered == check)
                    .map(|(_, at)| *at)
                    .max();
                ran.entry((check.id, check.environment))
                    .or_insert(LastRun { ago, at });
            }
        }
    }
    ran
}

/// Where a check was last answered on main's line ([`last_runs`]).
#[derive(Clone, Copy, Debug)]
struct LastRun {
    /// How many landings back from the tip.
    ago: usize,
    /// G822: when the newest record of the check there was written, in Unix seconds.
    at: Option<u64>,
}

/// G822 (a fleet finding, 2026-11-19: three runners served one environment, and each ran each
/// post check on the same tip within seconds of the others, before any had recorded the answer
/// [`answered_elsewhere`] reads; in a day, 23.0 of 49.1 post runner-hours were second and third
/// runs of one check on one tip): the other live runner that runs `entry` on main's tip, if one
/// does, so `me` does not. Of the runners that are members now ([`crate::presence::members`])
/// and serve the check's environment, and whose producer the check lists when it lists any,
/// `me` among them, the check goes to the one whose name weighs most with it ([`weight`],
/// rendezvous hashing): it stays with one runner while that runner stays live, and only the checks
/// of a runner that leaves move.
///
/// `None`, so `me` runs it, when no runner answered the check on main's newest [`RAN_SPAN`]
/// landings (`last`), or one of the check's own timeouts has passed since both its newest answer
/// and main's tip (`landed`, the tip's commit time): a runner that reads live and does not run
/// the check (its worker busy or down, say) leaves it to the others after that long.
///
/// G829 (a fleet finding, 2026-11-20: a check last answered 14 min before a new tip landed ran
/// on the tip on both runners of its environment, 4 min after it landed, under a 15 min timeout):
/// the timeout counted from the check's newest answer alone, so once that answer was older than
/// the timeout when a runner came to the tip, every runner ran the check there. It counts from
/// the tip's landing too, the first moment the check's owner could run it on the tip.
fn left_to(
    entry: &crate::policy::CheckEntry,
    (me, live): (&str, &[Value]),
    last: Option<&LastRun>,
    (landed, now): (Option<u64>, u64),
) -> Option<String> {
    let at = last?.at?;
    if now.saturating_sub(at.max(landed.unwrap_or(0))) >= entry.timeout_s {
        return None;
    }
    let owner = owner_of(entry, me, live);
    (owner != me).then(|| owner.to_owned())
}

/// G822: the runner that runs `entry`: of `me` and the runners whose presence reads live
/// (`live`) and serves the check's environment, and whose producer the check lists when it lists
/// any, the one whose name weighs most with it ([`weight`]). G857 elects the runner that
/// bisects a red of the check by it too ([`bisected_by`]).
fn owner_of<'a>(entry: &crate::policy::CheckEntry, me: &'a str, live: &'a [Value]) -> &'a str {
    let listed = |record: &Value| {
        entry.producers.as_ref().is_none_or(|producers| {
            record["producer"]
                .as_str()
                .is_some_and(|producer| producers.iter().any(|it| it == producer))
        })
    };
    live.iter()
        .filter(|record| crate::presence::serves(record, &entry.environment) && listed(record))
        .filter_map(|record| record["executor"].as_str())
        .chain([me])
        .max_by_key(|runner| (weight(entry, runner), *runner))
        .unwrap_or(me)
}

/// G822: `runner`'s rendezvous weight for `entry`: the first eight bytes of the SHA-256 of the
/// check, its environment and the runner's name, so every runner, of any version that has it,
/// weighs the same.
fn weight(entry: &crate::policy::CheckEntry, runner: &str) -> u64 {
    rendezvous(&entry.id, &entry.environment, runner)
}

/// [`weight`], by the check's id and environment: G826 elects the runner that tells a red of
/// the check by it too (`crate::red_owner`).
pub(crate) fn rendezvous(check: &str, environment: &str, runner: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{check}\0{environment}\0{runner}"));
    let mut first = [0_u8; 8];
    first.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(first)
}

/// G625 (E87, a fleet finding, 2026-11-01: two runner boxes served one environment, and each
/// ran every post check on each tip: 62 runs on 46 tips in three hours, no verdict different,
/// 36 of them after the other box had recorded its answer): the checks of `entries` that another
/// producer's `records` on `commit`'s tree already answer, as the red judgment counts them (the
/// producers a check lists, verified under the registry, this policy's manifest, and `commit`
/// for a check keyed by it), with a verdict that is not unknown. Only a record already there
/// answers, so two runners that start one run together would both run it: G822 ([`left_to`])
/// leaves each check to one of them.
///
/// # Errors
///
/// A class file cannot be written.
fn answered_elsewhere(
    git: &Git,
    (registry, writer, policy): (Option<&Registry>, &Writer, &crate::policy::Policy),
    entries: &[&crate::policy::CheckEntry],
    (commit, records): (&str, Vec<EvidenceRecord>),
) -> Result<Vec<CheckId>, String> {
    let ids: Vec<CheckId> = entries
        .iter()
        .map(|entry| CheckId::new(&entry.id, &entry.environment))
        .collect();
    let others: Vec<EvidenceRecord> = records
        .into_iter()
        .filter(|record| {
            record.producer.name != writer.name
                && record.kind == evidence::KIND_CHECK
                && ids.contains(&record.check)
        })
        .collect();
    if others.is_empty() {
        return Ok(Vec::new());
    }
    let counted = policy.answering(evidence::counted(git, others, registry)?);
    Ok(ids
        .into_iter()
        .filter(|id| {
            // A post check runs on main's own commit (`crate::policy::CheckEntry::key`).
            let key = policy.key(id, (commit, None));
            evidence::combine(&counted, id, &policy.manifest_digest, key)
                != evidence::Combined::Unknown
        })
        .collect())
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
/// change does not touch, so a change runs only the checks it reaches. One piece for each
/// environment none of `taken` names, with every such check of it.
fn warm(repo: &Repo, taken: &BTreeSet<String>, fresh: &mut Fresh) -> Result<Planned, String> {
    if !repo.config.warm() {
        return Ok((Vec::new(), Vec::new()));
    }
    let git = &repo.git;
    let tip = fresh.tip(repo)?;
    let Some(policy) = crate::policy::load_at(git, &tip).map_err(String::from)? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let served = check::Served::read(git);
    let warmed = |entry: &&crate::policy::CheckEntry| {
        entry.stage == crate::policy::Stage::Pre
            && entry.footprint == crate::policy::FootprintMode::Observed
            && served.serves(&entry.environment)
            && !taken.contains(&entry.environment)
    };
    if !policy.checks.iter().any(|entry| warmed(&entry)) {
        return Ok((Vec::new(), Vec::new()));
    }
    let keys = Keys::load(git);
    fresh.registry(repo);
    fresh.evidence(repo, &keys, false);
    let registry = Registry::current(git)?;
    let tree = tree_of(git, &tip)?;
    let writer = Writer::of(git, &keys, registry.as_ref())?;
    let mut ids: BTreeMap<String, Vec<CheckId>> = BTreeMap::new();
    for entry in policy.checks.iter().filter(warmed) {
        let id = CheckId::new(&entry.id, &entry.environment);
        if writer.records(git, &tree, &id)? == 0 {
            ids.entry(entry.environment.clone()).or_default().push(id);
        }
    }
    let pieces = ids
        .into_iter()
        .map(|(environment, ids)| Piece {
            environment,
            kind: "warm",
            job: Job::Warm {
                tip: tip.clone(),
                ids,
            },
            keys: keys.clone(),
            writer: writer.clone(),
        })
        .collect();
    Ok((Vec::new(), pieces))
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

/// G244d (`[runner] bisect_post`): after the queue's work, never ahead of it, the culprit of
/// each red post check over more than one landing. One piece for each environment this box
/// serves that none of `taken` names, with every such red check of it; a check that cannot be
/// bisected here is its row.
fn culprits(repo: &Repo, taken: &BTreeSet<String>, fresh: &mut Fresh) -> Result<Planned, String> {
    if !repo.config.bisect_post() {
        return Ok((Vec::new(), Vec::new()));
    }
    let git = &repo.git;
    let trunk = fresh.tip(repo)?;
    let served = check::Served::read(git);
    // BACKLOG 113: a box that can run none of the policy's post checks finds no culprit, so it
    // judges nothing for them.
    let policy = match crate::policy::load_at(git, &trunk) {
        Ok(Some(policy)) => policy,
        // An invalid policy has no post check to bisect, as the red judgement reads it.
        Ok(None) | Err(crate::policy::LoadError::Invalid(_)) => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(crate::policy::LoadError::Git(why)) => return Err(why),
    };
    if !serves_a_post_check(&policy, &served, taken) {
        return Ok((Vec::new(), Vec::new()));
    }
    let keys = Keys::load(git);
    fresh.registry(repo);
    fresh.evidence(repo, &keys, false);
    let registry = Registry::current(git)?;
    let judgement = crate::red::judged(git, &trunk, registry.as_ref())?;
    let live = fresh.live(repo);
    let now = crate::presence::reader_now();
    let mut rows = Vec::new();
    let mut reds: BTreeMap<String, Vec<Red>> = BTreeMap::new();
    for red in judgement.red {
        let Some(suspect) = red.suspect.as_ref() else {
            continue;
        };
        if suspect.landings.len() < 2 && !unconfirmed(git, &red, suspect)? {
            continue;
        }
        if let Some(base) = &suspect.base_failed {
            let why = format!(
                "the check failed at {} as well, the base of its range, so the red is the \
                 check's own and no merge is bisected",
                base.named()
            );
            rows.push(not_run(&red, &why, &[]));
        } else if !served.serves(&red.environment) {
            rows.push(not_run(
                &red,
                "this box does not serve the check's environment",
                &[],
            ));
        } else if let (None, Some(base)) = (&suspect.passed, suspect.landings.first())
            && ran_on(git, &Writer::of(git, &keys, registry.as_ref())?, &red, base)?
        {
            let why = format!(
                "this box ran the check at {}, the base of its range, and the run decided nothing",
                base.named()
            );
            rows.push(not_run(&red, &why, &[]));
        } else if !taken.contains(&red.environment) {
            let entry = policy
                .checks
                .iter()
                .find(|entry| entry.id == red.check && entry.environment == red.environment);
            match bisected_by(
                git,
                entry,
                (&red, suspect),
                (&keys.executor_name, &live),
                now,
            )? {
                Some(owner) => rows.push(not_run(&red, &format!("runner {owner} bisects it"), &[])),
                None => reds.entry(red.environment.clone()).or_default().push(red),
            }
        }
    }
    let mut pieces = Vec::new();
    for (environment, reds) in reds {
        pieces.push(Piece {
            environment,
            kind: "bisect",
            job: Job::Bisect(reds),
            keys: keys.clone(),
            writer: Writer::of(git, &keys, registry.as_ref())?,
        });
    }
    Ok((rows, pieces))
}

/// BACKLOG 348: whether this box ran `red`'s check on `base`, the base of a range with no pass,
/// already. Its run there decided nothing (an infra failure, say), or the range would have a
/// good end or be the check's own, so it is not run again, as a bisect step that decided
/// nothing is not ([`bisect`]).
fn ran_on(git: &Git, writer: &Writer, red: &Red, base: &Mark) -> Result<bool, String> {
    let id = CheckId::new(&red.check, &red.environment);
    Ok(writer.records(git, &tree_of(git, &base.landing)?, &id)? > 0)
}

/// G857 (a fleet finding, 2026-11-22: two runners that served one environment each ran the same
/// bisect runs of a red post check, about 40 % of its bisect runs in an afternoon): the other live
/// runner that bisects `red` (its `suspect` range), if one does, so `me` does not. It is the
/// runner that runs the check on main's tip ([`owner_of`], G822), so one red is bisected on one
/// box while that box reads live, and moves only when it leaves.
///
/// `None`, so `me` bisects, when the policy at main has no entry for the check, or no producer
/// answered the check on a landing of the range or after it for twice its timeout: one timeout for
/// the owner to come to it, one for its run. A runner that reads live and does not bisect (its
/// worker busy with the queue, say) leaves the red to the others after that long.
fn bisected_by(
    git: &Git,
    entry: Option<&crate::policy::CheckEntry>,
    (red, suspect): (&Red, &Suspect),
    (me, live): (&str, &[Value]),
    now: u64,
) -> Result<Option<String>, String> {
    let Some(entry) = entry else {
        return Ok(None);
    };
    let owner = owner_of(entry, me, live);
    if owner == me {
        return Ok(None);
    }
    let quiet = last_answered(git, red, suspect)?
        .is_none_or(|at| now.saturating_sub(at) >= entry.timeout_s.saturating_mul(2));
    Ok((!quiet).then(|| owner.to_owned()))
}

/// G857: when a producer last answered `red`'s check on a landing of its `suspect` range or
/// after it, in Unix seconds: its bisect runs and its failures there.
fn last_answered(git: &Git, red: &Red, suspect: &Suspect) -> Result<Option<u64>, String> {
    let id = CheckId::new(&red.check, &red.environment);
    let mut landings: Vec<&str> = suspect
        .landings
        .iter()
        .chain(&suspect.failing)
        .chain([&suspect.newest_failed])
        .map(|mark| mark.landing.as_str())
        .collect();
    landings.sort_unstable();
    landings.dedup();
    let trees = landings
        .iter()
        .map(|landing| tree_of(git, landing))
        .collect::<Result<Vec<String>, String>>()?;
    let trees: Vec<&str> = trees.iter().map(String::as_str).collect();
    Ok(evidence::records_for_many(git, &trees)?
        .into_iter()
        .flatten()
        .filter(|record| record.kind == evidence::KIND_CHECK && record.check == id)
        .filter_map(|record| crate::clock::unix_of(&record.at))
        .max())
}

/// G627: whether the culprit of `suspect`, a range of one landing, stands on fewer than two
/// records of `red`'s check in its tree, of any producer: one rerun there confirms it or makes
/// it flaky ([`bisect`]). With two, it ran again already, and is never run a third time here.
fn unconfirmed(git: &Git, red: &Red, suspect: &Suspect) -> Result<bool, String> {
    let Some(culprit) = &suspect.culprit else {
        return Ok(false);
    };
    let id = CheckId::new(&red.check, &red.environment);
    let records = evidence::records_for(git, &tree_of(git, &culprit.mark.landing)?)?;
    let count = records
        .iter()
        .filter(|record| record.kind == evidence::KIND_CHECK && record.check == id)
        .count();
    Ok(count < 2)
}

/// The producer this box records as, and its signer when the repository has a registry.
#[derive(Clone)]
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
    if let (None, Some(base)) = (&suspect.passed, suspect.landings.first()) {
        return on_base(repo, (keys, writer), (red, &id), window, base);
    }
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
            (&mark.landing, None, &[]),
            Under::FirstParent,
        )? {
            Ok(answer) => answer.verdict,
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
                (&mark.landing, None, &[]),
                Under::FirstParent,
            )? {
                Ok(answer) => answer.verdict,
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

/// BACKLOG 348: the one run of `red`'s check (`id`) on `base`, the oldest landing of a range
/// with no pass, as a change of nothing on it under its own policy: the landing that added the
/// check has no first parent whose policy lists it. A pass makes it the range's good end, and
/// the next pass bisects above it; a failure makes the red the check's own (`crate::red`).
fn on_base(
    repo: &Repo,
    (keys, writer): (&Keys, &Writer),
    (red, id): (&Red, &CheckId),
    window: Value,
    base: &Mark,
) -> Result<Value, String> {
    let verdict = match run_at(
        repo,
        keys,
        writer,
        (id, ("bisect", window)),
        (&base.landing, None, &[]),
        Under::Itself,
    )? {
        Ok(answer) => answer.verdict,
        Err(why) => return Ok(not_run(red, &why, &[])),
    };
    let outcome = match verdict {
        Verdict::Pass => "base_passed",
        Verdict::ProductFailure => "base_failed",
        Verdict::InfraFailure | Verdict::Unknown => "undecided",
    };
    Ok(json!({
        "check": red.check,
        "environment": red.environment,
        "outcome": outcome,
        "culprit": null,
        "tested": [tested_json(base, verdict)],
        "runs": 1,
        "bound": 1,
    }))
}

/// How long at most a check that no worker answered on this box waits before this box tries it
/// again (BACKLOG 117, a fleet finding, 2026-10-28: the one post runner waited 383-400 s on each of
/// eight worker tickets that never started, and wrote no record, so the same check came first
/// in every pass and its other post checks were silent for two hours). A record of the check
/// ends the wait.
const UNAVAILABLE_WAIT: std::time::Duration = std::time::Duration::from_hours(1);

/// How long at least such a check waits (G633, a fleet finding, 2026-11-02: one try that ended
/// unanswered after 12 s held a post check for the whole hour). Between the two, it waits
/// [`UNAVAILABLE_COST_FACTOR`] times what its try cost, so tries no worker answers take at most a
/// tenth of the box's time. It is longer than [`HOLDING_UNAVAILABLE_WAIT`], the wait `run_at`
/// checks again, so `tips` alone decides a green check's wait.
const UNAVAILABLE_FLOOR: std::time::Duration = std::time::Duration::from_mins(10);

/// How many times its try's cost a check no worker answered waits.
const UNAVAILABLE_COST_FACTOR: u64 = 10;

/// How long a check that holds changes waits instead: a red check, or one whose fix waits for its
/// run (G573, a fleet finding, 2026-10-29: a scripts check's fix waited for its run, one worker did
/// not answer it, and ten changes on its paths waited the whole hour for the next try). Each
/// other post check of its environment still gets the passes in between.
const HOLDING_UNAVAILABLE_WAIT: std::time::Duration = std::time::Duration::from_mins(5);

/// [`HOLDING_UNAVAILABLE_WAIT`] when the check `holds` changes, else [`UNAVAILABLE_COST_FACTOR`]
/// times `cost_s`, from [`UNAVAILABLE_FLOOR`] to [`UNAVAILABLE_WAIT`]; a test build honours
/// `BAY_TEST_HOLDING_UNAVAILABLE_WAIT_S` and `BAY_TEST_UNAVAILABLE_FLOOR_S`, a release build never
/// does.
fn unavailable_wait(holds: bool, cost_s: u64) -> std::time::Duration {
    let knob = |var: &str, wait: std::time::Duration| {
        if crate::version::TEST_HOOKS
            && let Some(secs) = std::env::var(var).ok().and_then(|secs| secs.parse().ok())
        {
            return std::time::Duration::from_secs(secs);
        }
        wait
    };
    if holds {
        return knob(
            "BAY_TEST_HOLDING_UNAVAILABLE_WAIT_S",
            HOLDING_UNAVAILABLE_WAIT,
        );
    }
    let floor = knob("BAY_TEST_UNAVAILABLE_FLOOR_S", UNAVAILABLE_FLOOR);
    std::time::Duration::from_secs(cost_s.saturating_mul(UNAVAILABLE_COST_FACTOR))
        .max(floor)
        .min(UNAVAILABLE_WAIT)
}

/// When no worker answered a check on this box, to the Unix second, and what that try cost.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Unanswered {
    at: u64,
    cost_s: u64,
}

/// The file where this box notes the checks no worker answered: `<check>@<environment>` to its
/// [`Unanswered`].
fn unavailable_file(repo: &Repo) -> std::path::PathBuf {
    repo.common_dir.join("bay").join("post-unavailable.json")
}

fn unavailable_key(id: &CheckId) -> String {
    format!("{}@{}", id.id, id.environment)
}

/// The notes in [`unavailable_file`]; none when it cannot be read, so a check is tried once
/// more.
fn unavailable_notes(repo: &Repo) -> std::collections::BTreeMap<String, Unanswered> {
    std::fs::read_to_string(unavailable_file(repo))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Records that no worker answered `id` (`Some`), or, with `None`, that it has a record again.
/// One piece at a time reads and writes the file (#123), so no piece drops another's note.
fn note_unavailable(repo: &Repo, id: &CheckId, now: Option<Unanswered>) {
    static NOTING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _noting = NOTING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// The seconds `id` still waits, when no worker answered it here within [`unavailable_wait`];
/// shorter when the check `holds` changes.
fn waits_for_a_worker(repo: &Repo, id: &CheckId, holds: bool) -> Option<u64> {
    let note = *unavailable_notes(repo).get(&unavailable_key(id))?;
    let left = note
        .at
        .saturating_add(unavailable_wait(holds, note.cost_s).as_secs())
        .saturating_sub(crate::live::unix_now());
    (left > 0).then_some(left)
}

/// The policy a run on a landing is judged under.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Under {
    /// The landing's first parent's, as its post check is (G91): a post run and a bisect step.
    FirstParent,
    /// The landing's own, as a change of nothing on it: a warm run on the tip, and the run on the
    /// base of a red's range (BACKLOG 348), whose first parent's policy may not list the check.
    Itself,
}

/// Runs `red`'s post check on landing `commit` `under` a policy and records its verdict as this
/// box's post evidence on the landing's tree, pushed. `Ok(Err(why))` when it cannot run here.
fn run_at(
    repo: &Repo,
    keys: &Keys,
    writer: &Writer,
    (id, why): (&CheckId, (&'static str, Value)),
    (commit, since, rerun): (&str, Option<&str>, &[String]),
    under: Under,
) -> Result<Result<Answer, String>, String> {
    let (field, body) = why;
    // A post run and a bisect step judge a red check (a post run of a green one passed the
    // longer wait in `tips`); a warm run holds nothing.
    if let Some(left) = waits_for_a_worker(repo, id, field != "warm") {
        return Ok(Err(format!(
            "no worker answered the check here lately; it is tried again in {left} s"
        )));
    }
    let git = &repo.git;
    // A warm run, and the run on a red's base, is a candidate of no change on its commit, judged
    // as one on it: its base is the commit itself. A post run and a bisect step judge a landing
    // under its first parent.
    let warm = field == "warm";
    let base = match under {
        Under::Itself => commit.to_owned(),
        Under::FirstParent => rev(git, &format!("{commit}^1"))?,
    };
    let tree = tree_of(git, commit)?;
    let base_tree = tree_of(git, &base)?;
    let stage = if warm {
        crate::policy::Stage::Pre
    } else {
        crate::policy::Stage::Post
    };
    let mut only = match alone(git, writer, id, &base, stage)? {
        Ok(only) => only,
        Err(why) => return Ok(Err(why)),
    };
    // A bisect step asks whether one landing fails, once: its own rerun at the suspect rules
    // out a flake ([`bisect`]), so a post check's retries (0.6.6) are for its run on the tip.
    if field == "bisect" {
        for entry in &mut only.checks {
            entry.retry_on_failure = 0;
        }
    }
    // A post run on the tip names what changed since `since` (G439); a bisect step names its
    // landing's own change, which is what it judges.
    let changed = check::changed_paths(git, since.unwrap_or(&base), commit)?;
    let run = writer.records(git, &tree, id)? + 1;
    let short = commit.get(..12).unwrap_or(commit);
    // A fleet finding (2026-10-27): the id named no check, so the second post check on a tip
    // took the first one's evidence id, its record was refused as a second record under one id,
    // and every later post check of that tip never ran: four of five were silent for 22 hours.
    // #123: two checks of one id in two environments run side by side on one tip, so the id,
    // and the scratch it names, names the environment too.
    let word = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let candidate_id = format!(
        "{field}-{}-{}-{short}-{run}",
        word(&id.id),
        word(&id.environment)
    );
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
    let started = std::time::Instant::now();
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
            // G821 (a fleet finding, 2026-11-19): an adapter that picks tests by changed paths
            // keeps its whole suite for a bisect step, whose range is not the one it judges.
            turn: Some(field),
            rerun,
            retry: 0,
        },
        &writer.name,
        &check::Limits {
            budget: None,
            finished: Vec::new(),
            asked: Vec::new(),
            heartbeat: None,
            hidden: check::hidden_for(repo, keys),
            sandbox: check::Sandbox::here().reading(git),
            // Bisection asks whether each commit fails, whole: no base excuses a failure.
            baseline: None,
            parallel: 1,
            halt: None,
        },
        &mut check::Hooks {
            keep: &mut keep,
            beat: &mut || {},
            seen: None,
        },
    );
    let _ = check::remove_tree(&scratch);
    answered(repo, writer, id, (ran?, started.elapsed()))
}

/// What `ran` answered for `id` in `took`: its verdict, its record pushed; or why it gave none. A
/// check no worker answers is noted with what its try cost, so this box waits before it tries it
/// again (BACKLOG 117, G633).
fn answered(
    repo: &Repo,
    writer: &Writer,
    id: &CheckId,
    (ran, took): (check::Ran, std::time::Duration),
) -> Result<Result<Answer, String>, String> {
    if let Some((_, reason)) = ran.unavailable {
        let note = Unanswered {
            at: crate::live::unix_now(),
            cost_s: took.as_secs(),
        };
        note_unavailable(repo, id, Some(note));
        return Ok(Err(format!("no worker answers the check: {reason}")));
    }
    let Some(answer) = Answer::of(&ran.records) else {
        return Ok(Err("the check wrote no record".to_owned()));
    };
    note_unavailable(repo, id, None);
    let notes = evidence::notes_ref(&writer.name);
    let _writing = evidence::writing();
    repo.git
        .run(&["push", "-q", repo.remote(), &format!("{notes}:{notes}")])
        .map_err(|error| {
            format!(
                "the post check's record is written here but not pushed: {}",
                error.message()
            )
        })?;
    Ok(Ok(answer))
}

/// What one run of a check answered on its tree, its reruns with it, as evidence combines a
/// tree's records: a `product_failure` with a later `pass` is `flaky` (0.6.6: a post check's
/// retry that passed), a `product_failure` alone a failure, a `pass` alone a pass, and a run
/// that neither passed nor failed its last record's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Answer {
    verdict: Verdict,
    flaky: bool,
}

impl Answer {
    fn of(records: &[EvidenceRecord]) -> Option<Self> {
        let last = records.last()?.verdict;
        let failed = records
            .iter()
            .any(|record| record.verdict == Verdict::ProductFailure);
        let passed = records.iter().any(|record| record.verdict == Verdict::Pass);
        let verdict = if failed {
            Verdict::ProductFailure
        } else if passed {
            Verdict::Pass
        } else {
            last
        };
        Some(Self {
            verdict,
            flaky: failed && passed,
        })
    }

    /// The word a post work row says: the verdict, or `flaky`.
    fn word(self) -> Value {
        if self.flaky {
            json!("flaky")
        } else {
            json!(self.verdict)
        }
    }
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
    entry.history_paths = entry.when_paths.take();
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

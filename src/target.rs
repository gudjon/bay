//! NEW-5 and NEW-9 (owner ruling 2026-10-20): promotion targets and their freeze windows.
//!
//! A target (`[[target]]` in `.bay/config.toml`) is a ref such as `refs/heads/staging` that an
//! operator moves to a commit already on its source: the trunk, or another target
//! (`production` from `staging`). A promotion is not a landing: it makes no tree, no merge and
//! no `Change-Id`, so it needs no queue, marker or trunk segment of its own, and deployment
//! stays outside bay (`DESIGN` § 13). bay names no verb for it: a plain
//! `git push origin <commit>:refs/heads/staging`, judged here.
//!
//! A commit is promotable to a target now when:
//!
//! - it is on the first-parent line of the source's tip, or (N-20) a bay landing an outside
//!   merge on that line pushed off it ([`crate::trunkline::on_line`]), and moves the target by
//!   a fast-forward;
//! - every `[[check]]` of `.bay/policy.toml` whose id the target `requires` combines to `pass`
//!   for it, judged as a post check is (`crate::publish::post_answers`): its records on the
//!   commit's tree, counted under the registry, under the manifest of the policy at its first
//!   parent. A required id the policy does not declare is unknown, never green;
//! - its committer time is at least `soak_s` old;
//! - no `[[hold]]` of the target is in force, or its landing carries `Fixes:` or `Reverts:`.
//!
//! The rule is read from the config at the trunk's tip, and the `requires` and `soak_s` of the
//! same target in the config at the target's own tip are added to it (the G26 analogue): a
//! commit never weakens the criteria it is judged by. Freezes are read at the trunk's tip only,
//! so one lifted there is lifted at once.
//!
//! Detection (`bay status`, the fleet map, `bay status --since`) reads the remote's target refs into
//! [`crate::names::TARGET`] and names, per target, its tip, whether that tip is on the source's
//! first-parent line, the commits on it the source does not reach (`drift`), how far it lags,
//! the required checks at its tip, the freeze in force, and the newest commit of the source
//! that is promotable now. On a bay host the scoped hook (`crate::hook`) refuses a push that
//! moves a target to a commit that is not promotable, with `PROMOTION_REFUSED`.
//!
//! N-7: a target with `advance = true` is moved by the executor as well ([`advance`]): each pass
//! fast-forwards it to that newest promotable commit, judged again by [`judge`] before the push,
//! and pushed under a lease on the tip it read, so it never moves backwards, sideways or over
//! another writer's move. A target promoted from another one advances from that target's tip
//! as this pass left it. A target without `advance` is only ever moved by an operator.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::config::{Config, Freeze, Target};
use crate::evidence::{self, CheckId, Combined};
use crate::git::Git;
use crate::registry::Registry;
use crate::repo::Repo;

/// How many first-parent commits of a source the search for a promotable commit reads back.
pub const WINDOW: usize = crate::red::DEFAULT_WINDOW;

/// The most drift commits a row names; `drift.count` counts them all.
pub const DRIFT_NAMED: usize = 5;

/// The event of a target that moved to a commit its rule holds for.
pub const MOVED: &str = "target_moved";

/// The event of a target that moved to, or sits at, a commit its rule does not hold for.
pub const UNPROMOTABLE: &str = "target_unpromotable";

/// The rule one target is judged by ([module doc](self)).
#[derive(Clone, Debug)]
pub struct Rule {
    pub target: Target,
    /// The ref of the source: the trunk's, or another target's.
    pub from_ref: String,
    /// The source's name: the trunk's name, or another target's.
    pub from: String,
    pub requires: Vec<String>,
    pub soak_s: u64,
    pub freezes: Vec<Freeze>,
}

impl Rule {
    /// The rule of `target` as `config` (the trunk tip's) defines it, with what the same target
    /// requires in `at_tip`, the config at the target's own tip, added.
    #[must_use]
    pub fn new(config: &Config, target: &Target, at_tip: Option<&Config>) -> Self {
        let trunk = config.trunk();
        let (from, from_ref) = match target
            .from_target(trunk)
            .and_then(|from| config.target(from))
        {
            Some(source) => (source.name.clone(), source.reference()),
            None => (trunk.to_owned(), format!("refs/heads/{trunk}")),
        };
        let mut requires = target.requires.clone();
        let mut soak_s = target.soak();
        if let Some(before) = at_tip.and_then(|config| config.target(&target.name)) {
            for id in &before.requires {
                if !requires.contains(id) {
                    requires.push(id.clone());
                }
            }
            soak_s = soak_s.max(before.soak());
        }
        Self {
            target: target.clone(),
            from_ref,
            from,
            requires,
            soak_s,
            freezes: config
                .freezes
                .iter()
                .filter(|freeze| freeze.target == target.name)
                .cloned()
                .collect(),
        }
    }

    /// The freeze in force at `now`, the one that ends last.
    #[must_use]
    pub fn freeze(&self, now: u64) -> Option<&Freeze> {
        self.freezes
            .iter()
            .filter(|freeze| freeze.active(now))
            .max_by_key(|freeze| crate::clock::unix_of(&freeze.end).unwrap_or(u64::MAX))
    }
}

/// A commit of a source's first-parent line, with what the time rules read.
struct Candidate {
    commit: String,
    /// Its committer time.
    committed: u64,
    /// Whether its landing carries `Fixes:` or `Reverts:`.
    fixes: bool,
}

/// The format of one [`Candidate`] line of `git log`.
const CANDIDATE_FORMAT: &str = "--format=%H%x00%ct%x00%(trailers:key=Fixes,valueonly,separator=%x2c)%x00%(trailers:key=Reverts,valueonly,separator=%x2c)%x01";

fn candidates(git: &Git, args: &[&str]) -> Result<Vec<Candidate>, String> {
    let mut all = vec!["log", CANDIDATE_FORMAT];
    all.extend(args);
    let text = git.run(&all).map_err(|error| error.message())?;
    Ok(text
        .split('\u{1}')
        .filter_map(|entry| {
            let mut fields = entry.trim_start_matches('\n').split('\0');
            let commit = fields.next()?.trim().to_owned();
            let committed = fields.next()?.trim().parse().ok()?;
            let fixes = fields.any(|value| !value.trim().is_empty());
            (!commit.is_empty()).then_some(Candidate {
                commit,
                committed,
                fixes,
            })
        })
        .collect())
}

/// N-20: the commits of `from_tip`'s landing line ([`crate::trunkline`]) above `tip`, newest
/// first, within [`WINDOW`] first-parent commits: a bay landing an outside merge pushed off
/// the first-parent line is a candidate at that merge, when it fast-forwards `tip`.
fn line_candidates(git: &Git, from_tip: &str, tip: Option<&str>) -> Result<Vec<Candidate>, String> {
    let line = crate::trunkline::walk(
        git,
        from_tip,
        WINDOW,
        tip,
        "%ct%x1f%(trailers:key=Fixes,valueonly,separator=%x2c)%x1f%(trailers:key=Reverts,valueonly,separator=%x2c)",
    )?;
    let mut out = Vec::new();
    for entry in line.entries {
        if entry.via.is_some()
            && let Some(tip) = tip
            && !git.is_ancestor(tip, &entry.commit)?
        {
            continue;
        }
        let [committed, lineage @ ..] = entry.extra.as_slice() else {
            continue;
        };
        let Ok(committed) = committed.parse::<u64>() else {
            continue;
        };
        let fixes = lineage.iter().any(|value| !value.is_empty());
        out.push(Candidate {
            commit: entry.commit,
            committed,
            fixes,
        });
    }
    Ok(out)
}

/// Whether `commit` (a full object id) is on the first-parent line of `tip`: following first
/// parents from `tip` as many times as there are first-parent commits not reachable from
/// `commit` lands on it.
///
/// # Errors
///
/// A git fault.
pub fn on_first_parent(git: &Git, tip: &str, commit: &str) -> Result<bool, String> {
    let count = git
        .run(&[
            "rev-list",
            "--first-parent",
            "--count",
            tip,
            &format!("^{commit}"),
        ])
        .map_err(|error| error.message())?;
    let count: u64 = count
        .trim()
        .parse()
        .map_err(|_| format!("git rev-list --count printed {count:?}"))?;
    Ok(git
        .run(&[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{tip}~{count}^{{commit}}"),
        ])
        .is_ok_and(|at| at == commit))
}

fn count(git: &Git, args: &[&str]) -> Result<u64, String> {
    let mut all = vec!["rev-list", "--count"];
    all.extend(args);
    let text = git.run(&all).map_err(|error| error.message())?;
    text.trim()
        .parse()
        .map_err(|_| format!("git rev-list --count printed {text:?}"))
}

/// The state of each check `requires` names for `commit`, as `{check, environment, state}`:
/// `pass`, `product_failure`, `flaky`, `unknown` (only records that answer nothing),
/// `pending` (no record) or `not_required` (a machine check whose `when_paths` miss every path
/// the commit changes), judged under the policy at the commit's first parent; `unknown_check`
/// for an id that policy does not declare, and `no_policy` when there is no valid one.
///
/// # Errors
///
/// A git fault, or a policy or note that cannot be read (G104b).
pub fn checks(
    git: &Git,
    commit: &str,
    requires: &[String],
    registry: Option<&Registry>,
) -> Result<Vec<Value>, String> {
    if requires.is_empty() {
        return Ok(Vec::new());
    }
    let base = format!("{commit}^1");
    let has_base = git
        .run(&["rev-parse", "--verify", "-q", &format!("{base}^{{commit}}")])
        .is_ok();
    let policy = if has_base {
        match crate::policy::load_at(git, &base) {
            Ok(policy) => policy,
            Err(crate::policy::LoadError::Invalid(_)) => None,
            Err(crate::policy::LoadError::Git(why)) => return Err(why),
        }
    } else {
        None
    };
    let Some(policy) = policy else {
        return Ok(requires
            .iter()
            .map(|id| json!({"check": id, "environment": null, "state": "no_policy"}))
            .collect());
    };
    let tree = git
        .run(&["rev-parse", &format!("{commit}^{{tree}}")])
        .map_err(|error| error.message())?;
    let changed = crate::check::changed_paths(git, &base, commit)?;
    // G163: only records from the producers a check lists answer it.
    let mut records = policy.answering(evidence::counted(
        git,
        evidence::records_for(git, &tree)?,
        registry,
    )?);
    // G292: an approval of an earlier candidate of the commit's change that still holds
    // answers here as it answered the landing.
    let earlier = crate::throughput::change_candidates(git, commit);
    if !earlier.is_empty() {
        let base_tree = git
            .run(&["rev-parse", &format!("{base}^{{tree}}")])
            .map_err(|error| error.message())?;
        let carried = crate::throughput::carried_approvals(
            git,
            &policy,
            registry,
            (tree.as_str(), base_tree.as_str()),
            &records,
            &earlier,
        )?;
        records.extend(carried.records);
    }
    let mut out = Vec::new();
    for id in requires {
        let entries: Vec<&crate::policy::CheckEntry> = policy
            .checks
            .iter()
            .filter(|check| check.id == *id)
            .collect();
        if entries.is_empty() {
            out.push(json!({"check": id, "environment": null, "state": "unknown_check"}));
        }
        for entry in entries {
            // As post-merge judgement reads it: a check the commit does not wake is not
            // required of it. A person's approval is never implied by the paths (G288).
            if !entry.is_approval() && policy.unwoken(entry, &changed).is_some() {
                out.push(json!({"check": entry.id, "environment": entry.environment, "state": "not_required"}));
                continue;
            }
            let check = CheckId::new(&entry.id, &entry.environment);
            let subject = policy.commit_keyed(&check).then_some(commit);
            let state = match evidence::combine(&records, &check, &policy.manifest_digest, subject)
            {
                Combined::Pass => "pass",
                Combined::ProductFailure => "product_failure",
                Combined::Flaky => "flaky",
                Combined::Unknown => {
                    let recorded = records.iter().any(|record| {
                        record.kind == evidence::KIND_CHECK
                            && record.check == check
                            && record.manifest_digest == policy.manifest_digest
                            && subject.is_none_or(|commit| record.subject.commit == commit)
                    });
                    if recorded { "unknown" } else { "pending" }
                }
            };
            out.push(json!({"check": entry.id, "environment": entry.environment, "state": state}));
        }
    }
    Ok(out)
}

/// A failed rule as a row: `{"rule", "why", …}`.
fn failure(rule: &str, why: String, more: Value) -> Value {
    let mut row = json!({"rule": rule});
    row["why"] = Value::String(why);
    if let (Some(row), Value::Object(more)) = (row.as_object_mut(), more) {
        row.extend(more);
    }
    row
}

/// The check, soak and freeze rules `candidate` fails for `rule` at `now`. Unless `every`, a
/// candidate that already fails the freeze or the soak is not judged on its checks: the caller
/// needs only whether it is promotable, and its checks are the judgement's git reads, one set
/// a candidate of the soak window.
fn time_and_checks(
    git: &Git,
    rule: &Rule,
    candidate: &Candidate,
    registry: Option<&Registry>,
    now: u64,
    every: bool,
) -> Result<Vec<Value>, String> {
    let name = &rule.target.name;
    let commit = &candidate.commit;
    let mut failed = Vec::new();
    if let Some(freeze) = rule.freeze(now)
        && !candidate.fixes
    {
        failed.push(failure(
            "freeze",
            format!(
                "{name} is frozen until {} ({}), and the merge of {commit} carries no Fixes: or Reverts:",
                freeze.end, freeze.reason
            ),
            json!({"end": freeze.end, "reason": freeze.reason}),
        ));
    }
    let age = now.saturating_sub(candidate.committed);
    if age < rule.soak_s {
        failed.push(failure(
            "soak",
            format!(
                "{commit} has soaked {age}s of the {}s {name} requires",
                rule.soak_s
            ),
            json!({"soak_s": rule.soak_s, "remaining_s": rule.soak_s - age}),
        ));
    }
    if !every && !failed.is_empty() {
        return Ok(failed);
    }
    let rows = checks(git, commit, &rule.requires, registry)?;
    failed.extend(check_failures(rule, commit, &rows));
    Ok(failed)
}

/// A `check` failure for each row of [`checks`] that is not a pass.
fn check_failures(rule: &Rule, commit: &str, rows: &[Value]) -> Vec<Value> {
    rows.iter()
        .filter(|row| row["state"] != "pass" && row["state"] != "not_required")
        .map(|row| {
            let check = row["check"].as_str().unwrap_or_default();
            let state = row["state"].as_str().unwrap_or_default();
            failure(
                "check",
                format!(
                    "check {check} is {state} for {commit}, and {} requires it to pass",
                    rule.target.name
                ),
                json!({"check": check, "environment": row["environment"], "state": state}),
            )
        })
        .collect()
}

/// The `not_on_from` failure of `commit`.
fn off_source(rule: &Rule, commit: &str) -> Value {
    failure(
        "not_on_from",
        format!(
            "{commit} is not on the first-parent line of {} ({})",
            rule.from, rule.from_ref
        ),
        json!({"from": rule.from}),
    )
}

/// Every rule that a move of the target from `old` (`None`: it does not exist yet) to `commit`
/// fails, with `from_tip` the source's tip: empty when the move is a promotion.
///
/// # Errors
///
/// A git fault, or evidence or a policy that cannot be read.
pub fn judge(
    git: &Git,
    rule: &Rule,
    from_tip: Option<&str>,
    old: Option<&str>,
    commit: &str,
    registry: Option<&Registry>,
    now: u64,
) -> Result<Vec<Value>, String> {
    let mut failed = Vec::new();
    let reference = rule.target.reference();
    match from_tip {
        Some(tip) if crate::trunkline::on_line(git, tip, commit)? => {}
        _ => failed.push(off_source(rule, commit)),
    }
    if let Some(old) = old
        && old != commit
        && !git.is_ancestor(old, commit)?
    {
        failed.push(failure(
            "not_fast_forward",
            format!("{commit} does not fast-forward {reference} from {old}"),
            json!({"old": old}),
        ));
    }
    let Some(candidate) = candidates(git, &["-1", commit])?.into_iter().next() else {
        return Err(format!("{commit} is not a commit here"));
    };
    failed.extend(time_and_checks(git, rule, &candidate, registry, now, true)?);
    Ok(failed)
}

/// The refusal text of a move that fails `failed`: the code, the ref, and each failing rule.
#[must_use]
pub fn refusal(rule: &Rule, failed: &[Value]) -> String {
    format!(
        "{}: {} moves only to a checked commit on {}: {}",
        crate::result::Reason::PromotionRefused.code(),
        rule.target.reference(),
        rule.from,
        failed
            .iter()
            .filter_map(|row| row["why"].as_str())
            .collect::<Vec<_>>()
            .join("; ")
    )
}

/// The local copy of target `name`'s remote ref.
#[must_use]
pub fn local_ref(name: &str) -> String {
    format!("{}{name}", crate::names::TARGET)
}

/// Fetches each target's ref from the remote into [`local_ref`], removing the copy of one the
/// remote lacks. Returns whether the remote answered: offline, the copies are what the last
/// fetch saw.
pub fn fetch(repo: &Repo) -> bool {
    let targets = &repo.config.targets;
    if targets.is_empty() {
        return true;
    }
    let refs: Vec<String> = targets.iter().map(Target::reference).collect();
    let mut args = vec!["ls-remote", repo.remote()];
    args.extend(refs.iter().map(String::as_str));
    let Ok(listed) = repo.git.run(&args) else {
        return false;
    };
    let held: BTreeMap<&str, &str> = listed
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.split_once('\t')?;
            Some((name, oid))
        })
        .collect();
    let mut refspecs = Vec::new();
    for target in targets {
        let reference = target.reference();
        if held.contains_key(reference.as_str()) {
            refspecs.push(format!("+{reference}:{}", local_ref(&target.name)));
        } else {
            let _ = repo
                .git
                .run(&["update-ref", "-d", &local_ref(&target.name)]);
        }
    }
    if refspecs.is_empty() {
        return true;
    }
    let mut args = vec!["fetch", "-q", "--no-tags", repo.remote()];
    args.extend(refspecs.iter().map(String::as_str));
    repo.git.run(&args).is_ok()
}

/// The tip of each target's local copy, by name; `None` for one the remote did not hold.
///
/// # Errors
///
/// A git fault.
pub fn tips(git: &Git, config: &Config) -> Result<BTreeMap<String, Option<String>>, String> {
    config
        .targets
        .iter()
        .map(|target| {
            git.ref_target(&local_ref(&target.name))
                .map(|tip| (target.name.clone(), tip))
                .map_err(|error| error.message())
        })
        .collect()
}

/// The rule of `target`, with the config at its tip read (G104b: one that cannot be read is
/// named, never read as none).
///
/// # Errors
///
/// The config at the target's tip cannot be read.
pub fn rule_at(
    git: &Git,
    config: &Config,
    target: &Target,
    tip: Option<&str>,
) -> Result<Rule, String> {
    let at_tip = tip.map(|tip| Config::at(git, tip)).transpose()?;
    Ok(Rule::new(config, target, at_tip.as_ref()))
}

/// `detail.targets` of `bay status` and the fleet map: one row per target, read from the local
/// copies of the target refs and the trunk commit `trunk` (the [module doc](self)). A row bay
/// cannot judge says why under `unread`, never reads as promotable.
///
/// # Errors
///
/// A git fault.
pub fn rows(
    git: &Git,
    config: &Config,
    trunk: &str,
    registry: Option<&Registry>,
    now: u64,
) -> Result<Vec<Value>, String> {
    let tips = tips(git, config)?;
    let read = Read {
        git,
        config,
        trunk,
        tips: &tips,
        registry,
        now,
    };
    let mut out = Vec::new();
    for target in &config.targets {
        let tip = tips.get(&target.name).cloned().flatten();
        let mut row = read
            .row(target, tip.as_deref())
            .unwrap_or_else(|why| json!({"unread": why}));
        row["name"] = json!(target.name);
        row["ref"] = json!(target.reference());
        out.push(row);
    }
    Ok(out)
}

/// What one reading of the targets shares.
struct Read<'a> {
    git: &'a Git,
    config: &'a Config,
    trunk: &'a str,
    tips: &'a BTreeMap<String, Option<String>>,
    registry: Option<&'a Registry>,
    now: u64,
}

impl Read<'_> {
    fn row(&self, target: &Target, tip: Option<&str>) -> Result<Value, String> {
        let git = self.git;
        let rule = rule_at(git, self.config, target, tip)?;
        let from_tip = if rule.from == self.config.trunk() {
            Some(self.trunk.to_owned())
        } else {
            self.tips.get(&rule.from).cloned().flatten()
        };
        let freeze = rule
            .freeze(self.now)
            .map(|freeze| json!({"end": freeze.end, "reason": freeze.reason}));
        let mut row = json!({
            "from": rule.from,
            "from_tip": from_tip,
            "tip": tip,
            "requires": rule.requires,
            "soak_s": rule.soak_s,
            "freeze": freeze,
            "on_from": null,
            "drift": null,
            "lag": null,
            "checks": [],
            "tip_failures": [],
        });
        let mut on_from = false;
        if let (Some(tip), Some(from)) = (tip, from_tip.as_deref()) {
            on_from = crate::trunkline::on_line(git, from, tip)?;
            let range = format!("{from}..{tip}");
            let drift = count(git, &[&range])?;
            let named: Vec<String> = if drift == 0 {
                Vec::new()
            } else {
                git.run(&["rev-list", &format!("--max-count={DRIFT_NAMED}"), &range])
                    .map_err(|error| error.message())?
                    .lines()
                    .map(str::to_owned)
                    .collect()
            };
            row["on_from"] = json!(on_from);
            row["drift"] = json!({"count": drift, "commits": named});
            row["lag"] = json!(count(git, &["--first-parent", from, &format!("^{tip}")])?);
        }
        if let Some(tip) = tip {
            // Detection, as G85 is for trunk: the rules the tip fails whatever the time of
            // reading.
            let checks = checks(git, tip, &rule.requires, self.registry)?;
            let mut failed = Vec::new();
            if !on_from {
                failed.push(off_source(&rule, tip));
            }
            failed.extend(check_failures(&rule, tip, &checks));
            row["checks"] = json!(checks);
            row["tip_failures"] = json!(failed);
        }
        row["promotable"] = self.promotable(&rule, from_tip.as_deref(), tip, on_from)?;
        Ok(row)
    }

    /// The newest commit of the source's first-parent line, above the target's tip and within
    /// [`WINDOW`], that is promotable now: `{"commit", "behind_from"}`, or `{"commit": null,
    /// "blocked_by"}` naming why the newest one is not, so the operator sees what to wait for.
    /// A tip off its source's first-parent line (`on_from` false) is named instead: no commit
    /// above it on that line is known to fast-forward it.
    fn promotable(
        &self,
        rule: &Rule,
        from_tip: Option<&str>,
        tip: Option<&str>,
        on_from: bool,
    ) -> Result<Value, String> {
        let Some(from_tip) = from_tip else {
            let why = format!("{} has no tip", rule.from);
            return Ok(
                json!({"commit": null, "blocked_by": [{"rule": "not_on_from", "why": why}]}),
            );
        };
        if tip == Some(from_tip) {
            return Ok(json!({"commit": null, "blocked_by": [], "at_from": true}));
        }
        if tip.is_some() && !on_from {
            let why = format!(
                "{} is off the first-parent line of {}: no commit of that line is known to fast-forward it",
                rule.target.reference(),
                rule.from
            );
            return Ok(
                json!({"commit": null, "blocked_by": [{"rule": "not_fast_forward", "why": why}]}),
            );
        }
        let mut newest = None;
        for (behind, candidate) in line_candidates(self.git, from_tip, tip)?.iter().enumerate() {
            // The newest candidate names every rule it fails; an older one only whether it
            // fails one.
            let failed = time_and_checks(
                self.git,
                rule,
                candidate,
                self.registry,
                self.now,
                newest.is_none(),
            )?;
            if failed.is_empty() {
                return Ok(json!({"commit": candidate.commit, "behind_from": behind}));
            }
            if newest.is_none() {
                newest = Some(failed);
            }
        }
        Ok(json!({"commit": null, "blocked_by": newest.unwrap_or_default()}))
    }
}

/// N-7: one executor pass's moves of the targets with `advance = true`, sources first, each a
/// fast-forward of the remote ref to the newest commit of its source that is promotable now
/// (`promotable` of [`rows`]), at `now`, with `trunk` the trunk's tip. Each move is a row
/// `{target, ref, from, to}` (`from` is `null` for a ref the remote did not hold); one the
/// judgement or the remote refused also names `error`. A target with nothing promotable
/// above its tip is not named.
///
/// # Errors
///
/// The remote did not answer for the target refs, a git fault, or a config at a target's tip
/// that cannot be read.
pub fn advance(
    repo: &Repo,
    trunk: &str,
    registry: Option<&Registry>,
    now: u64,
) -> Result<Vec<Value>, String> {
    let config = &repo.config;
    if !config.targets.iter().any(|target| target.advance) {
        return Ok(Vec::new());
    }
    if !fetch(repo) {
        return Err(format!(
            "the remote {} did not answer for the target refs, so no target advanced",
            repo.remote()
        ));
    }
    let git = &repo.git;
    let mut tips = tips(git, config)?;
    let mut order: Vec<&Target> = config.targets.iter().collect();
    order.sort_by_key(|target| depth(config, target));
    let mut moves = Vec::new();
    for target in order {
        if !target.advance {
            continue;
        }
        let tip = tips.get(&target.name).cloned().flatten();
        let rule = rule_at(git, config, target, tip.as_deref())?;
        let from_tip = if rule.from == config.trunk() {
            Some(trunk.to_owned())
        } else {
            tips.get(&rule.from).cloned().flatten()
        };
        let on_from = match (tip.as_deref(), from_tip.as_deref()) {
            (Some(tip), Some(from)) => crate::trunkline::on_line(git, from, tip)?,
            _ => false,
        };
        let read = Read {
            git,
            config,
            trunk,
            tips: &tips,
            registry,
            now,
        };
        let promotable = read.promotable(&rule, from_tip.as_deref(), tip.as_deref(), on_from)?;
        let Some(commit) = promotable["commit"].as_str().map(str::to_owned) else {
            continue;
        };
        if tip.as_deref() == Some(commit.as_str()) {
            continue;
        }
        let reference = target.reference();
        let mut row = json!({"target": target.name, "ref": reference, "from": tip, "to": commit});
        // The judgement the hook makes of an operator's push, made again: a fast-forward on the
        // source's first-parent line, every required check green, soaked, and no freeze.
        let failed = judge(
            git,
            &rule,
            from_tip.as_deref(),
            tip.as_deref(),
            &commit,
            registry,
            now,
        )?;
        if !failed.is_empty() {
            row["error"] = json!(refusal(&rule, &failed));
            moves.push(row);
            continue;
        }
        // A compare-and-swap on the tip this pass read: absent when the remote held none.
        let lease = format!(
            "--force-with-lease={reference}:{}",
            tip.as_deref().unwrap_or_default()
        );
        let refspec = format!("{commit}:{reference}");
        match git.run(&["push", "-q", &lease, repo.remote(), &refspec]) {
            Ok(_) => {
                let _ = git.run(&["update-ref", &local_ref(&target.name), &commit]);
                tips.insert(target.name.clone(), Some(commit));
            }
            Err(error) => row["error"] = json!(error.message()),
        }
        moves.push(row);
    }
    Ok(moves)
}

/// How many targets lie between `target` and the trunk on its `from` chain, so a source sorts
/// before the targets promoted from it. [`Config::parse`] refuses a cycle; the count still
/// stops at the number of targets.
fn depth(config: &Config, target: &Target) -> usize {
    let trunk = config.trunk();
    let mut depth = 0;
    let mut source = target;
    while depth < config.targets.len()
        && let Some(next) = source
            .from_target(trunk)
            .and_then(|from| config.target(from))
    {
        depth += 1;
        source = next;
    }
    depth
}

/// G85's detection for targets: the `target_moved` or `target_unpromotable` event of a target
/// whose tip is now `tip`, from `rows`' row of it.
#[must_use]
pub fn event(row: &Value) -> Option<Value> {
    let tip = row["tip"].as_str()?;
    let failures = row["tip_failures"].as_array()?;
    let kind = if failures.is_empty() {
        MOVED
    } else {
        UNPROMOTABLE
    };
    let name = row["name"].as_str()?;
    Some(json!({
        "schema": crate::log::EVENT_SCHEMA,
        "id": format!("{kind}:{name}:{tip}"),
        "kind": kind,
        "target": name,
        "ref": row["ref"],
        "commit": tip,
        "from": row["from"],
        "failures": failures,
        "recurrence_key": format!("{kind}/{name}/{tip}"),
    }))
}

#[cfg(test)]
mod tests {
    use super::Rule;
    use crate::config::Config;

    #[test]
    fn the_config_at_the_targets_tip_only_adds_to_its_rule() {
        // The G26 analogue: a promoted commit never weakens the criteria it is judged by.
        let trunk = Config::parse(
            "[[target]]\nname = \"staging\"\nrequires = [\"a\"]\n\
             [[hold]]\ntarget = \"staging\"\nend = \"2026-12-24T00:00:00Z\"\nreason = \"r\"\n",
        )
        .unwrap();
        let tip = Config::parse(
            "[[target]]\nname = \"staging\"\nrequires = [\"b\", \"a\"]\nsoak_s = 60\n",
        )
        .unwrap();
        let staging = trunk.target("staging").unwrap();
        let rule = Rule::new(&trunk, staging, Some(&tip));
        assert_eq!(rule.requires, ["a", "b"]);
        assert_eq!(rule.soak_s, 60);
        assert_eq!(rule.from, "main");
        assert_eq!(rule.from_ref, "refs/heads/main");
        assert_eq!(rule.freezes.len(), 1, "freezes are the trunk's");
        let lifted = Rule::new(&tip, tip.target("staging").unwrap(), Some(&trunk));
        assert!(
            lifted.freezes.is_empty(),
            "a freeze lifted on trunk is lifted"
        );
        assert_eq!(lifted.requires, ["b", "a"]);
        let end = crate::clock::unix_of("2026-12-24T00:00:00Z").unwrap();
        assert!(rule.freeze(end - 1).is_some());
        assert!(rule.freeze(end).is_none());
    }
}

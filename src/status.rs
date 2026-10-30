//! `bay status`: current state, derived on read (`DESIGN` § 4, `INTERFACES` § 1).
//!
//! "Landed" comes from trunk, never from the chain alone: a change whose publication is on
//! trunk is landed even when its chain was not updated before a crash. Built here: one change
//! or every local change, with freshness, and each change's named inputs judged at trunk
//! (`DESIGN` § 8): `invalidated` while trunk requires a version it has not observed, `repairing`
//! while no candidate is validated under an observed one. Leftovers (G41) are a later item.
//! `bay status` also fetches the remote's `refs/bay/live/*` and records that fetch, from which
//! `--all` reads the fleet map ([`crate::fleet`], G23), failing closed without it (G49).
//! Completion is reported per dimension, `running` and `effective` from counted runtime
//! records ([`crate::runtime`]), with the selected profile's verdict ([`crate::completion`]).
//! After a fetch it accepts and acks this actor's talks ([`crate::talk::receive`], § 11c), and
//! says so in `detail.talk_accept`. Trunk commits that did not land through bay (merges and
//! pushes outside it) are named and counted under `detail.unattributed`
//! ([`crate::unattributed`], G85). With operations queued and no fresh executor presence on
//! record (`crate::presence`, G67), `detail.warnings` names `no_live_executor`. N-20: each merge
//! outside bay in the red window that pushed bay landings off trunk's first-parent line is named
//! there as `trunk_not_linear` ([`crate::trunkline`]). G309: `detail.flow` names the flow of
//! work, live changes by actor beside the WIP limit, landings, cycle time, the oldest live change
//! and the changes waiting for an approval, derived from the chains on read ([`crate::flow`]).

use std::path::Path;

use serde_json::{Value, json};

use crate::change::{CHANGES_PREFIX, Chain, ChangeRecord, ChangeState, change_ref};
use crate::clock::now_rfc3339;
use crate::completion::{self, Dimension, Need};
use crate::evidence::{self, EvidenceRecord};
use crate::inputs::Probe;
use crate::publish::scan;
use crate::receipts::{self, Receipt};
use crate::repo::Repo;
use crate::result::{BayResult, Freshness, Reason};
use crate::runtime::Confirmed;
use crate::view;

#[must_use]
pub fn status(dir: &Path, change: Option<&str>) -> BayResult {
    run(dir, change).unwrap_or_else(BayResult::failed)
}

/// What a cached read says instead of a fetch time: it never synchronized.
pub const CACHED: &str = "local cache (not fetched by this read)";

/// The state of the local changes `select` picks, read from this repository's own refs only
/// (`DESIGN` § 11: "Local caches and cursors are allowed; status shows its last
/// synchronization point"). Used by the agent hook, where latency is a measured interface: it
/// pushes no receipt, fetches nothing, and writes no view journal (leftovers are not read).
/// The trunk it judges against is the remote-tracking ref, the last synchronization point,
/// and `freshness.fetched_at` says it was not fetched. `select` sees each change's id and
/// record before any trunk scan, so changes it drops cost one chain read.
///
/// # Errors
///
/// No repository, no tracking ref of the remote trunk, or a git fault.
pub fn cached(
    repo: &Repo,
    select: &dyn Fn(&str, &ChangeRecord) -> bool,
) -> Result<BayResult, String> {
    let trunk = repo
        .git
        .run(&["rev-parse", "--verify", &repo.trunk_tracking_ref()])
        .map_err(|error| error.message())?;
    let pending = receipts::pending(repo);
    // W5: every chain in one batch (G200b), never one read per change.
    let chains = Chain::new(&repo.git)
        .read_all()
        .map_err(|error| error.message())?;
    let mut changes = Vec::new();
    for held in chains {
        let (id, record) = (held.id.as_str(), held.record);
        if !select(id, &record) {
            continue;
        }
        let unsynced: Vec<&Receipt> = pending
            .iter()
            .map(|(_, receipt)| receipt)
            .filter(|receipt| receipt.change == id)
            .collect();
        changes.push(describe(repo, &record, &trunk, &unsynced, &Read::Cached)?);
    }
    let mut result = BayResult::ok(json!({"changes": changes, "trunk": trunk}));
    result.freshness = Some(Freshness {
        trunk,
        fetched_at: CACHED.to_owned(),
    });
    Ok(result)
}

/// Fetches the remote trunk and, after it, the fleet refs; returns trunk's tip and the time of
/// the trunk fetch, taken before the fleet fetch so it never overstates trunk's freshness.
/// Offline, the tracking ref is the trunk and there is no fetch time.
fn synchronize(repo: &Repo) -> Result<(String, Option<String>), String> {
    match repo.fetch_trunk() {
        Ok(trunk) => {
            let fetched_at = now_rfc3339();
            crate::fleet::fetch(repo);
            Ok((trunk, Some(fetched_at)))
        }
        Err(_) => Ok((
            repo.git
                .run(&["rev-parse", "--verify", &repo.trunk_tracking_ref()])
                .map_err(|error| error.message())?,
            None,
        )),
    }
}

/// Reads each change of `ids` fresh from the remote ([`Repo::remote_chain`]): a fast-forward
/// brings what an executor recorded elsewhere, a retention cut replaces the local chain, and
/// local work the remote lacks is kept (G289).
fn refresh_chains(repo: &Repo, chain: &Chain, ids: &[String]) -> Result<(), String> {
    for id in ids {
        // Local work the remote lacks was pushed by the sync above, or is kept here.
        let _ = repo.remote_chain(id);
        // A change named here but not local is read from the remote.
        if chain.head(id).map_err(|error| error.message())?.is_none() {
            repo.fetch_refs(&[change_ref(id)]).map_err(|error| {
                format!(
                    "change {id} is not here or on the remote: {}",
                    error.message()
                )
            })?;
        }
    }
    Ok(())
}

/// The changes a status read names, their chains refreshed from the remote: the one named, or
/// every live one this repository holds (the 0.5.3 status patch, owner 2026-10-25). An ended
/// change (merged, abandoned, superseded, no effect) never comes back, so its record here is
/// final, and `bay status <change>` still reads it whole. A listing refreshes in one fetch.
fn listed(repo: &Repo, chain: &Chain, named: Option<String>) -> Result<Vec<String>, String> {
    crate::log::fetch(repo);
    if let Some(id) = named {
        let ids = vec![id];
        refresh_chains(repo, chain, &ids)?;
        return Ok(ids);
    }
    // W5: every chain's record in one batch (G200b); a chain it cannot read stays listed.
    let ended_here: std::collections::BTreeSet<String> = chain
        .read_all()
        .map_err(|error| error.message())?
        .into_iter()
        .filter(|held| ended(held.record.state))
        .map(|held| held.id)
        .collect();
    let ids: Vec<String> = repo
        .git
        .run(&["for-each-ref", "--format=%(refname)", CHANGES_PREFIX])
        .map_err(|error| error.message())?
        .lines()
        .map(|name| name.trim_start_matches(CHANGES_PREFIX).to_owned())
        .filter(|id| !ended_here.contains(id))
        .collect();
    let _ = repo.remote_chains(&ids);
    Ok(ids)
}

/// How many changes a listing describes; the rest are named in `more_changes`.
const LISTED: usize = 20;

/// The live changes a listing names but does not describe: how many, the first hundred ids, and
/// how to read one. `null` when none is left out.
fn more_changes(more: &[String]) -> Value {
    if more.is_empty() {
        return Value::Null;
    }
    json!({
        "count": more.len(),
        "ids": more.iter().take(LISTED).collect::<Vec<_>>(),
        "read": "bay status <change>",
    })
}

/// Whether a change in `state` has ended for good.
pub(crate) const fn ended(state: crate::change::ChangeState) -> bool {
    use crate::change::ChangeState;
    matches!(
        state,
        ChangeState::Landed
            | ChangeState::Abandoned
            | ChangeState::Superseded
            | ChangeState::NoEffect
    )
}

/// The records of `ids`, read in one batch of every chain (G200b), never one walk per change (a
/// fleet finding, 2026-10-26: 2,408 state reads, a third of a five-minute status). One named
/// change is read alone. A listing (`named` false) leaves out a copy of another change's chain,
/// which is no change (G563); a read of the copy alone names it.
fn held_records(chain: &Chain, ids: &[String], named: bool) -> Result<Vec<ChangeRecord>, String> {
    let mut held: std::collections::BTreeMap<String, ChangeRecord> = if ids.len() == 1 {
        std::collections::BTreeMap::new()
    } else {
        chain
            .read_all()
            .map_err(|error| error.message())?
            .into_iter()
            .map(|held| (held.id, held.record))
            .collect()
    };
    ids.iter()
        .filter_map(|id| match held.remove(id) {
            Some(record) => Some(Ok(record)),
            None => match chain.read(id) {
                Ok((_, record)) => Some(Ok(record)),
                Err(crate::change::ChainError::Copy { .. }) if !named => None,
                Err(error) => Some(Err(error.message())),
            },
        })
        .collect()
}

fn run(dir: &Path, change: Option<&str>) -> Result<BayResult, String> {
    let repo = match Repo::open_fetched(dir) {
        Ok(repo) => repo,
        Err(error) => return Ok(error.into_result()),
    };
    let _ = receipts::sync(&repo);
    let pending = receipts::pending(&repo);
    let (trunk, fetched_at) = synchronize(&repo)?;
    // NEW-4: the lane refs an unattributed landing is judged against, as the remote holds them.
    if fetched_at.is_some() {
        crate::lane::fetch(&repo);
    }
    // § 11c: a fetching command on the recipient's machine accepts and acks its talks.
    let talk_accept = fetched_at
        .is_some()
        .then(|| crate::talk::poll::command(&repo, crate::live::unix_now()))
        .flatten();
    let chain = Chain::new(&repo.git);
    let ids = listed(
        &repo,
        &chain,
        change.map(str::to_owned).or_else(|| repo.view_change(dir)),
    )?;
    // The runtime adapter is asked only for a change this read names: a listing of every
    // change stays bounded however many have landed.
    let named = change.is_some() || repo.view_change(dir).is_some();
    let read = Read::Full {
        registry: crate::registry::Registry::current(&repo.git)?,
        ask_runtime: ids.len() == 1 && named,
    };
    let mut changes = Vec::new();
    // An agent reads this answer whole, so it has a budget (a fleet finding, 2026-10-26: a
    // listing of 146 live changes ran to megabytes and minutes). At most LISTED changes are
    // described, the reader's own first; the rest are named, and `bay status <change>` reads one.
    let me = crate::change::current_actor(&repo.common_dir)
        .ok()
        .map(|actor| actor.name);
    let records = held_records(&chain, &ids, named)?;
    let queued: usize = records.iter().map(crate::presence::waiting).sum();
    let mut records = records;
    records.sort_by_key(|record| me.as_deref() != Some(record.actor.name.as_str()));
    // A listing describes the reader's own changes; everyone else's are counted and named, so
    // an agent's answer stays a few hundred tokens however busy the repository is.
    let mine_only = ids.len() > 1 && me.is_some();
    let mut more = Vec::new();
    for record in &records {
        let others = mine_only && me.as_deref() != Some(record.actor.name.as_str());
        if changes.len() >= LISTED || others {
            more.push(record.id.clone());
            continue;
        }
        let unsynced: Vec<&Receipt> = pending
            .iter()
            .map(|(_, receipt)| receipt)
            .filter(|receipt| receipt.change == record.id)
            .collect();
        changes.push(describe(&repo, record, &trunk, &unsynced, &read)?);
    }
    // Red, flakes and post checks judge the evidence notes: fetched once here, never left to a
    // landed change's row (the listing names live changes only, the 0.5.3 status patch).
    if fetched_at.is_some() {
        repo.fetch_evidence_once(&crate::signing::Keys::load(&repo.git));
    }
    // A read of one change scans its own view only, never every view on the machine (a fleet
    // finding, 2026-10-26: 43 views cost a named status about 26 s).
    let named = change.map(str::to_owned).or_else(|| repo.view_change(dir));
    let leftovers = leftovers(&repo, &trunk, named.as_deref(), me.as_deref())?;
    let quarantines = quarantines(&repo, &trunk);
    let flakes = flake_rows(&repo, &trunk, &read);
    let (red_rows_now, red_pending) = red_rows(&repo, &trunk);
    let targets = target_rows(&repo, &trunk, fetched_at.is_some(), &read);
    // G309: the flow of work, derived on read from the chains this repository holds.
    let flow = crate::flow::flow(&repo, &trunk).unwrap_or_else(|why| json!({"unread": why}));
    let mut result = BayResult::ok(json!({
        "changes": changes,
        "more_changes": more_changes(&more),
        "flakes": flakes,
        "flow": flow,
        "leftovers": leftovers,
        "quarantines": quarantines,
        "red": red_rows_now,
        "red_pending": red_pending,
        "talks": talk_rows(&repo),
        "targets": targets,
        "trunk": trunk,
        "unattributed": unattributed_rows(&repo, &trunk),
        // G305: whether bay's hook judges agents' Git commands in this repository: `on` when
        // the config at trunk declares an agent adapter (`bay doctor` checks this box has its
        // entries). Off, `unattributed` counts what went around bay.
        "guard": {"hook": if repo.config.claude_adapter() { "on" } else { "off" }},
    }));
    if let (Some(detail), Some(accept)) = (result.detail.as_mut(), talk_accept) {
        detail["talk_accept"] = accept;
    }
    // N-8: a held trunk is named, with where the hold comes from, its reason and its end.
    if let (Some(detail), Some(hold)) = (result.detail.as_mut(), crate::hold::held(&repo, &trunk)) {
        detail["hold"] = hold.to_json();
    }
    let mut warnings: Vec<Value> = crate::presence::no_live_executor(&repo, queued)
        .into_iter()
        .chain(crate::presence::unserved_environments(
            &repo, queued, &trunk,
        ))
        .collect();
    // N-20: each outside merge in the red window that pushed bay landings off the line.
    warnings.extend(not_linear(&repo.git, &trunk));
    // R11: delivery that fails is never silent.
    warnings.extend(crate::log::wake_failing(&repo.git));
    if let Some(detail) = result.detail.as_mut()
        && !warnings.is_empty()
    {
        detail["warnings"] = Value::from(warnings);
    }
    if let [only] = ids.as_slice() {
        result = result.with_change(only.clone());
    }
    result.freshness = Some(Freshness {
        trunk,
        fetched_at: fetched_at.unwrap_or_else(|| "not fetched (remote unreachable)".to_owned()),
    });
    Ok(result)
}

/// N-20: a `trunk_not_linear` warning for each merge outside bay among the newest `[red] window`
/// first-parent commits of trunk that put bay landings off the first-parent line: the merge,
/// its parents, how many landings it pushed off (and which), and the recovery hint. Those
/// landings are still found ([`crate::trunkline`]); the warning says the host let the merge in.
/// A config bay cannot read names none: `red` and `unattributed` say why already.
fn not_linear(git: &crate::git::Git, trunk: &str) -> Vec<Value> {
    crate::red::window(git, trunk)
        .and_then(|window| crate::trunkline::walk(git, trunk, window, None, ""))
        .map(|line| {
            line.merges
                .iter()
                .map(crate::trunkline::ForeignMerge::to_json)
                .collect()
        })
        .unwrap_or_default()
}

/// G47: the open talks this actor sent or must answer, each with who owes the reply, its ages
/// and its standing; a lost or stale talk stays listed. Unreadable talks list nothing here.
fn talk_rows(repo: &Repo) -> Value {
    let Ok(me) = crate::change::current_actor(&repo.common_dir) else {
        return json!([]);
    };
    crate::talk::standing::rows(&repo.git, &me.name, crate::talk::standing::reader_now())
        .map_or_else(|why| json!({"unread": why}), Value::from)
}

/// G85: the trunk commits that did not land through bay, named and counted. An unreadable
/// trunk lists none and says why.
fn unattributed_rows(repo: &Repo, trunk: &str) -> Value {
    crate::unattributed::scan(&repo.git, trunk).unwrap_or_else(|why| json!({"unread": why}))
}

/// NEW-5: one row per promotion target (`crate::target::rows`), after a fetch of the remote's
/// target refs and, when a target requires checks, of the other producers' evidence. Offline,
/// the rows are read from the copies of the last fetch, and say so. A judgement bay cannot make
/// says why under `unread`, never reads as promotable.
fn target_rows(repo: &Repo, trunk: &str, fetched: bool, read: &Read) -> Value {
    if repo.config.targets.is_empty() {
        return json!([]);
    }
    let fetched = fetched && crate::target::fetch(repo);
    if fetched
        && repo
            .config
            .targets
            .iter()
            .any(|target| !target.requires.is_empty())
    {
        repo.fetch_evidence_once(&crate::signing::Keys::load(&repo.git));
    }
    // An unreadable registry never reads as none: evidence would then count unsigned.
    let current;
    let registry = match read {
        Read::Full { registry, .. } => registry.as_ref(),
        Read::Cached => match crate::registry::Registry::current(&repo.git) {
            Ok(registry) => {
                current = registry;
                current.as_ref()
            }
            Err(why) => return json!({"unread": why}),
        },
    };
    match crate::target::rows(
        &repo.git,
        &repo.config,
        trunk,
        registry,
        crate::live::unix_now(),
    ) {
        Ok(rows) => rows
            .into_iter()
            .map(|mut row| {
                row["fetched"] = json!(fetched);
                row
            })
            .collect(),
        Err(why) => json!({"unread": why}),
    }
}

/// G244: the post checks trunk is red on, each owned by the landing that broke it and naming
/// its suspect range; and (NEW-6) those no landing in the window answers, `red_pending`, never
/// clear. A judgement bay cannot make (a policy, config or registry it cannot read, G104b)
/// lists none in either and says why: an unread trunk is never read as not red.
fn red_rows(repo: &Repo, trunk: &str) -> (Value, Value) {
    crate::registry::Registry::current(&repo.git)
        .and_then(|registry| crate::red::state(&repo.git, trunk, registry.as_ref()))
        .map_or_else(
            |why| (json!({"unread": why}), json!({"unread": why})),
            |judgement| judgement.to_json(),
        )
}

/// The open quarantines of the policy at trunk (ruling 8c), so a set-aside check is never
/// silent: each with its `owner`, its age in days from `since`, and whether it is `overdue`.
/// One past its end date stays in force and is shown as `overdue` (`DESIGN` § 7: an end never
/// changes a judgement by itself; the policy change that removes the entry does). An invalid policy
/// lists none here; `land` names it with `POLICY_INVALID`. A policy bay cannot read (G104b)
/// lists none and says why: an unread policy is never read as no quarantine.
fn quarantines(repo: &Repo, trunk: &str) -> Value {
    open_quarantines(&repo.git, trunk).map_or_else(|why| json!({"unread": why}), Value::from)
}

/// Per required check of the policy at trunk, pre-merge and post (0.6.6: a post check's retry
/// that passes turns no main red, so its flakes are counted here), how many of the newest
/// first-parent landings (the `[red] window`) carry runs of it that combine to `flaky`, read
/// from the evidence already in this clone: a check that flakes is seen before anyone
/// quarantines it. Each row names `landings`, how many were read, and the check's
/// `retry_on_failure` and `allow_flaky`. A policy, config or registry bay cannot read lists
/// none and says why.
fn flake_rows(repo: &Repo, trunk: &str, read: &Read) -> Value {
    // An unreadable registry never reads as none: evidence would then count unsigned.
    let current;
    let registry = match read {
        Read::Full { registry, .. } => registry.as_ref(),
        Read::Cached => match crate::registry::Registry::current(&repo.git) {
            Ok(registry) => {
                current = registry;
                current.as_ref()
            }
            Err(why) => return json!({"unread": why}),
        },
    };
    flakes(&repo.git, trunk, registry).map_or_else(|why| json!({"unread": why}), Value::from)
}

/// The rows of [`flake_rows`].
///
/// # Errors
///
/// The policy or the config at `trunk` cannot be read (G104b), a note is unreadable, or a git
/// fault.
fn flakes(
    git: &crate::git::Git,
    trunk: &str,
    registry: Option<&crate::registry::Registry>,
) -> Result<Vec<Value>, String> {
    let policy = match crate::policy::load_at(git, trunk) {
        Ok(Some(policy)) => policy,
        Ok(None) | Err(crate::policy::LoadError::Invalid(_)) => return Ok(Vec::new()),
        Err(crate::policy::LoadError::Git(why)) => return Err(why),
    };
    let required: Vec<&crate::policy::ManifestEntry> =
        policy.manifest.iter().chain(&policy.post).collect();
    if required.is_empty() {
        return Ok(Vec::new());
    }
    let window = crate::red::window(git, trunk)?.to_string();
    let landings = git
        .run(&[
            "log",
            "--first-parent",
            "-n",
            &window,
            "--format=%H %T",
            trunk,
        ])
        .map_err(|error| error.message())?;
    let mut counts = vec![0_u64; required.len()];
    let mut read = 0_u64;
    for (commit, tree) in landings.lines().filter_map(|line| line.split_once(' ')) {
        read += 1;
        let records = policy.answering(evidence::counted(
            git,
            evidence::records_for(git, tree)?,
            registry,
        )?);
        for (entry, count) in required.iter().zip(counts.iter_mut()) {
            let id = evidence::CheckId::new(&entry.id, &entry.environment);
            let subject = policy.commit_keyed(&id).then_some(commit);
            // Each landing was judged under the manifest of its own base: any one of them
            // under which its runs combine to flaky counts it once.
            let mut digests: Vec<&str> = records
                .iter()
                .filter(|record| record.check == id)
                .map(|record| record.manifest_digest.as_str())
                .collect();
            digests.sort_unstable();
            digests.dedup();
            if digests.iter().any(|digest| {
                evidence::combine(&records, &id, digest, subject) == evidence::Combined::Flaky
            }) {
                *count += 1;
            }
        }
    }
    Ok(required
        .iter()
        .zip(counts)
        .map(|(entry, flaky)| {
            let id = evidence::CheckId::new(&entry.id, &entry.environment);
            let check = policy.check(&id);
            json!({
                "check": entry.id,
                "environment": entry.environment,
                "flaky": flaky,
                "landings": read,
                "retry_on_failure": check.map_or(0, |check| check.retry_on_failure),
                "allow_flaky": check.is_some_and(|check| check.allow_flaky),
            })
        })
        .collect())
}

/// The open quarantines of the policy at `at`, each with whether it is overdue today; none
/// without a policy or with an invalid one. Read by `status` and by `bay doctor` (`DESIGN` § 7).
///
/// # Errors
///
/// The policy file cannot be read (G104b), or a git fault.
pub(crate) fn open_quarantines(git: &crate::git::Git, at: &str) -> Result<Vec<Value>, String> {
    let policy = match crate::policy::load_at(git, at) {
        Ok(Some(policy)) => policy,
        Ok(None) | Err(crate::policy::LoadError::Invalid(_)) => return Ok(Vec::new()),
        Err(crate::policy::LoadError::Git(why)) => return Err(why),
    };
    let now = now_rfc3339();
    let today = now.get(..10).unwrap_or_default();
    Ok(policy
        .quarantined
        .iter()
        .map(|quarantine| {
            json!({
                "check": quarantine.id,
                "environment": quarantine.environment,
                "reason": quarantine.reason,
                "end": quarantine.end,
                "owner": quarantine.owner,
                "since": quarantine.since,
                "age_days": quarantine.age_days(today),
                "overdue": quarantine.overdue(today),
            })
        })
        .collect())
}

/// Every view this machine still holds, with its change state, its age and whether it has
/// uncheckpointed edits (G41). No pull-request state, no chat activity.
fn leftovers(
    repo: &Repo,
    trunk: &str,
    only: Option<&str>,
    me: Option<&str>,
) -> Result<Vec<Value>, String> {
    // W5: every chain in one batch (G200b), never one read per view.
    let chain = Chain::new(&repo.git);
    let mut records: std::collections::BTreeMap<String, ChangeRecord> = chain
        .read_all()
        .map_err(|error| error.message())?
        .into_iter()
        .map(|held| (held.id, held.record))
        .collect();
    let mut out = Vec::new();
    for id in view::all(repo)
        .into_iter()
        .filter(|id| only.is_none_or(|only| only == id))
    {
        let Some(record) = records.remove(&id) else {
            continue;
        };
        // A listing reports the views of ended changes and of the reader's own live ones, at
        // most LISTED of them: checking every view on a shared box cost a status minutes (a
        // fleet finding, 2026-10-26: 256 views).
        let listed = ended(record.state) || me.is_none_or(|me| me == record.actor.name);
        if only.is_none() && (!listed || out.len() >= LISTED) {
            continue;
        }
        let landed = !scan(&repo.git, &record.base_first, trunk, &id)?
            .publications
            .is_empty();
        let state = if landed {
            json!("landed")
        } else {
            json!(record.state)
        };
        let path = repo.view_path(&id);
        let latest = chain.latest_draft(&id).map_err(|error| error.message())?;
        let uncheckpointed = if view::exists(&path) {
            json!(view::uncheckpointed(&path, &latest).map_err(|error| error.message())?)
        } else {
            json!(null)
        };
        // § 9a: after a landing, the view is releasable only when it holds no newer work than
        // the published revision. Newer work is kept, never cleaned away.
        let mut newer_work = false;
        if landed && view::exists(&path) {
            let published = last_revision(&record, &latest);
            newer_work =
                view::uncheckpointed(&path, &published).map_err(|error| error.message())?;
            let next = if newer_work {
                view::Journal::Published
            } else {
                view::Journal::Releasable
            };
            if view::journal(repo, &id) != Some(next) {
                view::set_journal(repo, &id, next)?;
            }
        }
        out.push(json!({
            "change": id,
            "view": path.display().to_string(),
            "state": state,
            "age_s": view::age_s(repo, &id),
            "uncheckpointed": uncheckpointed,
            "newer_work": newer_work,
            "journal": view::journal(repo, &id),
        }));
    }
    out.extend(stale_inflight(repo, &chain)?);
    Ok(out)
}

/// The revision a view is measured against once its change ended (§ 9a): the change's last
/// published operation's, else its `latest` draft. `leftovers` and the G97 release
/// (`crate::start`) both read it, so a view `status` calls releasable is one `start` may release.
#[must_use]
pub fn last_revision(record: &ChangeRecord, latest: &str) -> String {
    record
        .operations
        .iter()
        .rev()
        .find(|op| op.state == crate::change::OpState::Published)
        .map_or_else(|| latest.to_owned(), |op| op.revision.clone())
}

/// The in-flight refs this store still holds whose operation ended or is gone (G173): an
/// executor that was killed mid-run left them. Each names its ref, its operation and the
/// object it holds; deleting the ref is the cleanup. A ref of an operation still running is
/// that operation's, and is not listed.
fn stale_inflight(repo: &Repo, chain: &Chain) -> Result<Vec<Value>, String> {
    let listed = repo
        .git
        .run(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            crate::names::INFLIGHT_PREFIX,
        ])
        .map_err(|error| error.message())?;
    let mut out = Vec::new();
    for line in listed.lines() {
        let Some((name, object)) = line.split_once(' ') else {
            continue;
        };
        let op_id = name.trim_start_matches(crate::names::INFLIGHT_PREFIX);
        let change = op_id.split_once('.').map_or(op_id, |(change, _)| change);
        let op = chain
            .read(change)
            .ok()
            .and_then(|(_, record)| record.operations.into_iter().find(|op| op.op_id == op_id));
        if op.as_ref().is_some_and(|op| !op.state.is_terminal()) {
            continue;
        }
        out.push(json!({
            "kind": "inflight",
            "ref": name,
            "change": change,
            "op_id": op_id,
            "object": object,
            "op_state": op.map(|op| op.state),
        }));
    }
    Ok(out)
}

/// The counted records on `tree` whose id begins, after the producer, with `prefix`.
fn records_named(
    repo: &Repo,
    registry: Option<&crate::registry::Registry>,
    tree: &str,
    prefix: &str,
) -> Result<Vec<EvidenceRecord>, String> {
    let all = evidence::counted(&repo.git, evidence::records_for(&repo.git, tree)?, registry)?;
    Ok(all
        .into_iter()
        .filter(|record| {
            record
                .id
                .split_once('/')
                .is_some_and(|(_, rest)| rest.starts_with(prefix))
        })
        .collect())
}

/// Worker time on `records` (`ROADMAP` § 3): the wall time bay measured around each adapter
/// run and the time the adapter reported for its units, each summed. A record that carries no
/// time still counts, as `untimed`.
fn cost(records: &[EvidenceRecord]) -> Value {
    let round = |sum: f64| (sum * 1000.0).round() / 1000.0;
    json!({
        "records": records.len(),
        "untimed": records.iter().filter(|record| record.seconds.is_none()).count(),
        "worker_seconds": round(records.iter().filter_map(|record| record.seconds).sum()),
        "unit_seconds": round(
            records
                .iter()
                .flat_map(|record| &record.units)
                .filter_map(|unit| unit.seconds)
                .sum()
        ),
    })
}

/// How `describe` reads a change. The hook's path reads no evidence (its costs are `null`) and
/// only the input cache; a full read counts evidence under the registry and runs providers,
/// and asks the runtime adapter when `ask_runtime` (one named change).
enum Read {
    Cached,
    Full {
        registry: Option<crate::registry::Registry>,
        ask_runtime: bool,
    },
}

/// The cost of the change's last operation, over every record its attempts produced
/// (discarded attempts included), and of its landing, over the records on the landed tree that
/// the publishing operation made (`null` when not landed).
fn costs(
    repo: &Repo,
    record: &ChangeRecord,
    landed: Option<&crate::publish::Found>,
    registry: Option<&crate::registry::Registry>,
) -> Result<(Value, Value), String> {
    let mut op_records = Vec::new();
    for attempt in record.operations.last().map_or(&[][..], |op| &op.attempts) {
        let prefix = format!("{}-", attempt.candidate_id);
        op_records.extend(records_named(repo, registry, &attempt.tree, &prefix)?);
    }
    let publishing = landed.and_then(|found| {
        record
            .operations
            .iter()
            .rev()
            .find(|op| op.revision == found.revision)
            .map(|op| (found, op))
    });
    let landing = match publishing {
        Some((found, op)) => {
            let tree = repo
                .git
                .run(&["rev-parse", &format!("{}^{{tree}}", found.commit)])
                .map_err(|error| error.message())?;
            let prefix = format!("{}.", op.op_id);
            cost(&records_named(repo, registry, &tree, &prefix)?)
        }
        None => json!(null),
    };
    Ok((cost(&op_records), landing))
}

/// § 8: required versions are derived on read at trunk. Only a live change has live inputs: a
/// landed, superseded or abandoned one keeps its state, and no provider runs for it. A ready
/// change is live: a moved input takes back the readiness it claims.
fn live_inputs(
    repo: &Repo,
    record: &ChangeRecord,
    landed: bool,
    trunk: &str,
    probe: Probe,
) -> Result<Vec<crate::inputs::Judgement>, String> {
    let live = matches!(
        record.state,
        ChangeState::Open | ChangeState::Invalidated | ChangeState::Repairing | ChangeState::Ready
    );
    if landed || !live {
        return Ok(Vec::new());
    }
    crate::inputs::judge(repo, record, trunk, probe)
}

/// A one-off status cannot watch a lease; it reports a suspected expiry from the age of the
/// chain head and never claims (§ 9a).
fn lease(
    repo: &Repo,
    record: &ChangeRecord,
    op: &crate::change::Operation,
    published: bool,
) -> Value {
    if op.owner.is_none()
        || !matches!(
            op.state,
            crate::change::OpState::Running | crate::change::OpState::Checked
        )
        || published
    {
        return json!(null);
    }
    let age = repo
        .git
        .run(&[
            "log",
            "-1",
            "--format=%ct",
            &crate::change::change_ref(&record.id),
        ])
        .ok()
        .and_then(|time| time.trim().parse::<u64>().ok())
        .map(|time| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |now| now.as_secs())
                .saturating_sub(time)
        });
    json!(
        if age.is_some_and(|age| age >= repo.config.lease().as_secs()) {
            "suspected expired"
        } else {
            "held"
        }
    )
}

/// § 7: the completion dimensions, each from its own evidence, and the verdict of the profile
/// `[policy] profile` selects; never collapsed into "done". The hook's read reads no evidence,
/// so it confirms no runtime dimension and judges no profile (`profile` is `null`).
fn completion(
    repo: &Repo,
    record: &ChangeRecord,
    trunk: &str,
    landed: Option<&crate::publish::Found>,
    read: &Read,
) -> Result<Value, String> {
    let unchecked = landed.and_then(|found| unchecked(&repo.git, record, found));
    let checked = unchecked.is_none()
        && (landed.is_some()
            || record.operations.last().is_some_and(|op| {
                op.attempts
                    .last()
                    .is_some_and(|attempt| attempt.verdict == "pass")
            }));
    let mut out = completion_dimensions(repo, record, trunk, landed, read, checked)?;
    out["unchecked"] = json!(unchecked);
    Ok(out)
}

/// Why landed `found` was never checked, or `None` when it was: judged at its base by
/// [`crate::publish::unchecked_reason`] with the attempt that built it, fail closed.
fn unchecked(
    git: &crate::git::Git,
    record: &ChangeRecord,
    found: &crate::publish::Found,
) -> Option<&'static str> {
    let verdict = record
        .operations
        .iter()
        .flat_map(|op| op.attempts.iter())
        .find(|attempt| attempt.commit == found.commit)
        .map(|attempt| attempt.verdict.as_str());
    crate::publish::unchecked_reason(git, verdict, &found.commit)
}

/// The completion dimensions of [`completion`], with `checked` as judged there.
fn completion_dimensions(
    repo: &Repo,
    record: &ChangeRecord,
    trunk: &str,
    landed: Option<&crate::publish::Found>,
    read: &Read,
    checked: bool,
) -> Result<Value, String> {
    let Read::Full {
        registry,
        ask_runtime,
    } = read
    else {
        return Ok(json!({
            "merged": landed.is_some(),
            "checked": checked,
            "running": "unconfirmed",
            "effective": "unconfirmed",
            "runtime": null,
            "trust": null,
            "profile": null,
        }));
    };
    let (name, require) = selected_profile(repo, trunk, landed);
    // The adapter is asked only when its answer can change the verdict: the profile requires a
    // runtime dimension and a registry exists, without which nothing it prints could count.
    let needs_runtime = require.as_ref().is_ok_and(|profile| {
        profile.require.contains(&Need::Builtin(Dimension::Running))
            || profile
                .require
                .contains(&Need::Builtin(Dimension::Effective))
    });
    // N-21: under `signing = "approvals"` no producer signs, so nothing it prints counts.
    let signs_all = crate::registry::Registry::signing(registry.as_ref()).is_some();
    let ask = *ask_runtime && needs_runtime && signs_all;
    let (confirmed, runtime) = match landed {
        Some(found) => runtime(repo, record, found, registry.as_ref(), ask)?,
        None => (Confirmed::default(), json!(null)),
    };
    let mut met: Vec<Need> = [
        (Dimension::Published, landed.is_some()),
        (Dimension::Checked, checked),
        (Dimension::Running, !confirmed.running.is_empty()),
        (Dimension::Effective, !confirmed.effective.is_empty()),
    ]
    .into_iter()
    .filter_map(|(dimension, met)| met.then_some(Need::Builtin(dimension)))
    .collect();
    // G160: without a registry, or under `signing = "approvals"` (N-21), `checked` rests on
    // unsigned records alone. A runtime dimension never counts an unsigned record
    // ([`crate::runtime`]), and `published` is trunk's own fact.
    let mut unsigned: Vec<Need> = if !signs_all && checked {
        vec![Need::Builtin(Dimension::Checked)]
    } else {
        Vec::new()
    };
    // G150: the site's own dimensions, each judged for the landing; reported here and required
    // only through a profile, never in the landing verdict.
    let mut sites = serde_json::Map::new();
    for dimension in declared_dimensions(repo, trunk, landed) {
        let (state, records, rests_unsigned) = match landed {
            Some(found) => site_dimension(
                repo,
                &dimension,
                &record.id,
                found,
                trunk,
                registry.as_ref(),
            )?,
            None => ("missing", Vec::new(), false),
        };
        let need = Need::Site(dimension.name.clone());
        if matches!(state, "met" | "answered") {
            met.push(need.clone());
            if rests_unsigned {
                unsigned.push(need);
            }
        }
        sites.insert(
            dimension.name,
            json!({"state": state, "kind": dimension.kind, "producers": dimension.producers, "records": records}),
        );
    }
    let word = |ids: &[String]| {
        if ids.is_empty() {
            "unconfirmed"
        } else {
            "confirmed"
        }
    };
    Ok(json!({
        "merged": landed.is_some(),
        "checked": checked,
        "running": word(&confirmed.running),
        "effective": word(&confirmed.effective),
        "runtime": runtime,
        "trust": evidence::trust(registry.as_ref()),
        "dimensions": sites,
        "profile": completion::verdict(&name, require, &met, &unsigned),
    }))
}

/// G160: the evidence on the landed tree, or else on the last attempt's tree, each record named
/// `verified`, `unverifiable` or `unsigned`, under the result's `trust`. The hook's read reads
/// no evidence (`null`).
fn evidence_read(
    repo: &Repo,
    record: &ChangeRecord,
    landed: Option<&crate::publish::Found>,
    read: &Read,
) -> Result<Value, String> {
    let Read::Full { registry, .. } = read else {
        return Ok(json!(null));
    };
    let tree = match landed {
        Some(found) => Some(
            repo.git
                .run(&["rev-parse", &format!("{}^{{tree}}", found.commit)])
                .map_err(|error| error.message())?,
        ),
        None => record
            .operations
            .last()
            .and_then(|op| op.attempts.last())
            .map(|attempt| attempt.tree.clone()),
    };
    let Some(tree) = tree else {
        return Ok(json!(null));
    };
    let records = evidence::tagged(
        &repo.git,
        evidence::records_for(&repo.git, &tree)?,
        registry.as_ref(),
    )?;
    Ok(json!({
        "tree": tree,
        "trust": evidence::trust(registry.as_ref()),
        "records": records.iter().map(|(record, verified)| json!({
            "id": record.id,
            "kind": record.kind,
            "check": record.check.id,
            "verdict": record.verdict,
            "reasons": evidence::reasons(&record.units),
            "verified": verified,
        })).collect::<Vec<_>>(),
    }))
}

/// The selected profile's name and requirement, both read where the change is judged: at the
/// landing's accepted base once landed, else at trunk. Neither the work tree's config nor the
/// landing itself selects or relaxes the profile it is judged by. A config or policy bay
/// cannot read there makes the requirement unknown.
fn selected_profile(
    repo: &Repo,
    trunk: &str,
    landed: Option<&crate::publish::Found>,
) -> (String, Result<completion::Profile, String>) {
    let at = landed.map_or_else(|| trunk.to_owned(), |found| format!("{}^1", found.commit));
    let config = match crate::config::Config::at(&repo.git, &at) {
        Ok(config) => config,
        Err(why) => return (repo.config.profile().to_owned(), Err(why)),
    };
    let name = config.profile().to_owned();
    let require = match crate::policy::load_at(&repo.git, &at) {
        Ok(policy) => completion::requirement(
            &name,
            &policy.map(|policy| policy.profiles).unwrap_or_default(),
        ),
        Err(error) => Err(String::from(error)),
    };
    (name, require)
}

/// The `[[dimension]]` tables of the policy a change is judged by: at the landing's accepted
/// base once landed, else at trunk, as [`selected_profile`] reads it (G150).
fn declared_dimensions(
    repo: &Repo,
    trunk: &str,
    landed: Option<&crate::publish::Found>,
) -> Vec<completion::SiteDimension> {
    let at = landed.map_or_else(|| trunk.to_owned(), |found| format!("{}^1", found.commit));
    crate::policy::load_at(&repo.git, &at)
        .ok()
        .flatten()
        .map(|policy| policy.dimensions)
        .unwrap_or_default()
}

/// G150: one site dimension of a landing. Records count when they are of the dimension's kind,
/// from one of its producers, about that landing commit (never a tree alone: K11b), and, with a
/// registry, verify against the class file their kind names (K11c). A non-pass record is
/// answered only by a later landing whose `Fixes:`, `Reverts:` or `Supersedes:` names the
/// change, never by a later pass (G29, K11a). Returns the state (`met`, `answered`, `rejected`
/// or `missing`), the counted record ids, and whether they rest on unsigned records alone.
fn site_dimension(
    repo: &Repo,
    dimension: &completion::SiteDimension,
    change: &str,
    found: &crate::publish::Found,
    trunk: &str,
    registry: Option<&crate::registry::Registry>,
) -> Result<(&'static str, Vec<String>, bool), String> {
    let git = &repo.git;
    // The producers' notes refs, which this box may not hold yet; one that has none is no
    // error: its records are missing.
    for producer in &dimension.producers {
        let _ = repo.fetch_refs(&[evidence::notes_ref(producer)]);
    }
    let tree = git
        .run(&["rev-parse", &format!("{}^{{tree}}", found.commit)])
        .map_err(|error| error.message())?;
    let mut counted = Vec::new();
    for record in evidence::records_for(git, &tree)? {
        if record.kind != dimension.kind
            || !dimension.producers.contains(&record.producer.name)
            || record.subject.commit != found.commit
        {
            continue;
        }
        // N-21: under `signing = "approvals"` only an approval must verify.
        if let Some(registry) = registry
            && evidence::signed_kind(registry, &record.kind)
            && !record.verifies_in(git, registry)?
        {
            continue;
        }
        counted.push(record);
    }
    let ids = counted.iter().map(|record| record.id.clone()).collect();
    let unsigned = registry
        .is_none_or(|registry| !evidence::signed_kind(registry, &dimension.kind))
        && !counted.is_empty();
    if counted.is_empty() {
        return Ok(("missing", ids, false));
    }
    if counted
        .iter()
        .all(|record| record.verdict == evidence::Verdict::Pass)
    {
        return Ok(("met", ids, unsigned));
    }
    let answered = crate::lineage::RELATIONS.iter().any(|(_, trailer)| {
        git.run(&[
            "log",
            "--first-parent",
            "--format=%H",
            &format!("--grep=^{trailer}: {change}$"),
            &format!("{}..{trunk}", found.commit),
        ])
        .is_ok_and(|out| !out.trim().is_empty())
    });
    Ok((
        if answered { "answered" } else { "rejected" },
        ids,
        unsigned,
    ))
}

/// The counted runtime records on landed `found`'s tree. When `ask` and they do not confirm
/// both runtime dimensions, the runtime adapter is asked too, and what it printed that counts
/// is counted for this read; nothing is written. An adapter that fails confirms nothing and is
/// named, never read as a pass.
fn runtime(
    repo: &Repo,
    record: &ChangeRecord,
    found: &crate::publish::Found,
    registry: Option<&crate::registry::Registry>,
    ask: bool,
) -> Result<(Confirmed, Value), String> {
    let tree = repo
        .git
        .run(&["rev-parse", &format!("{}^{{tree}}", found.commit)])
        .map_err(|error| error.message())?;
    let mut records = evidence::records_for(&repo.git, &tree)?;
    let count = |records: &[EvidenceRecord]| {
        crate::runtime::confirmed(&repo.git, records.to_vec(), registry, &found.commit)
    };
    let mut confirmed = count(&records)?;
    let (mut adapter, mut error, mut refused) = ("not asked", None, Vec::new());
    if ask && (confirmed.running.is_empty() || confirmed.effective.is_empty()) {
        match crate::runtime::observe(repo, &record.id, &found.commit, &tree, registry) {
            Ok(None) => adapter = "unconfigured",
            Ok(Some(observed)) => {
                adapter = "ran";
                refused = observed.refused;
                records.extend(observed.kept);
                confirmed = count(&records)?;
            }
            Err(why) => {
                adapter = "failed";
                error = Some(why);
            }
        }
    }
    Ok((
        confirmed.clone(),
        json!({
            "running": confirmed.running,
            "effective": confirmed.effective,
            "adapter": adapter,
            "error": error,
            "refused": refused,
        }),
    ))
}

/// G91: the post-publication checks of landed `landed`, each pending until a record for its
/// tree arrives ([`crate::publish::post_checks`]); `null` when not landed, or on the hook's
/// read, which reads no evidence. A record arrives from a runner elsewhere, so while one is
/// pending the other producers' evidence is fetched first. A fault is named, never hidden.
fn post_checks(repo: &Repo, landed: Option<&crate::publish::Found>, read: &Read) -> Value {
    let (Some(found), Read::Full { registry, .. }) = (landed, read) else {
        return json!(null);
    };
    let judge = || crate::publish::post_checks(&repo.git, &found.commit, registry.as_ref());
    let pending = |rows: &[Value]| rows.iter().any(|row| row["state"] == "pending");
    let rows = match judge() {
        Ok(rows) if pending(&rows) => {
            repo.fetch_evidence_once(&crate::signing::Keys::load(&repo.git));
            judge()
        }
        other => other,
    };
    rows.map_or_else(|why| json!({"unread": why}), Value::from)
}

/// G245: the approvals the last operation of `record` waits on, when it is held with
/// `APPROVAL_PENDING` and the read is full: who can approve and the candidate to name, judged
/// like its land. An approver's clone may hold no registry yet: who can approve is then read
/// from the remote's, never reported as nobody.
fn approval_of(repo: &Repo, record: &ChangeRecord, read: &Read) -> Result<Option<Value>, String> {
    let Read::Full { registry, .. } = read else {
        return Ok(None);
    };
    approval_pending(repo, record, registry.as_ref())
}

/// G245: the `operation.approval` of `bay status` for `record`, read with `registry` (the
/// repository's, when it holds one): each approval check still owed, who can give it, the paths
/// it covers, and the candidate to name; `None` when the last operation is not held with
/// `APPROVAL_PENDING`. `bay waiting` and `bay approve` (G280, G281) read it here, so all three
/// name one candidate.
///
/// # Errors
///
/// A git fault reading the registry.
pub fn approval_pending(
    repo: &Repo,
    record: &ChangeRecord,
    registry: Option<&crate::registry::Registry>,
) -> Result<Option<Value>, String> {
    let Some(op) = record.operations.last() else {
        return Ok(None);
    };
    let held = op.outcome.as_deref().and_then(Reason::from_code) == Some(Reason::ApprovalPending);
    let Some(attempt) = op.attempts.last().filter(|_| held) else {
        return Ok(None);
    };
    let fetched = if registry.is_none() {
        repo.fetch_registry();
        crate::registry::Registry::current(&repo.git)?
    } else {
        None
    };
    // G292: an earlier candidate's approval holds while the files it covers are unchanged.
    let mut approval = match crate::publish::pending_approval(
        &repo.git,
        (
            attempt.base.as_str(),
            attempt.commit.as_str(),
            attempt.tree.as_str(),
        ),
        &crate::throughput::candidates_of(record),
        registry.or(fetched.as_ref()),
    ) {
        Ok(Some(approval)) => approval,
        Ok(None) => return Ok(None),
        Err(why) => return Ok(Some(json!({"unread": why}))),
    };
    // G281: the one command an approver runs to answer it.
    approval["approve"] = json!(format!("bay approve {}", record.id));
    Ok(Some(approval))
}

/// What an operation under way waits on, besides its place: § 7, a worker for the
/// environments no worker answered for; G177, a conflict with trunk the executor found while
/// the operation waits behind the queue's head, as `MERGE_CONFLICT` with its paths, so its
/// agent can refresh before the head reaches it.
fn waiting(op: &crate::change::Operation, operation: &mut Value) {
    if let Some(unavailable) = &op.unavailable {
        operation["waiting_for"] = json!("worker");
        operation["environments"] = json!(unavailable.environments);
    }
    if let Some(conflict) = &op.early_conflict {
        let reason = Reason::MergeConflict;
        operation["conflict"] = json!({
            "reason": reason.code(),
            "conflicts": conflict.paths,
            "trunk": conflict.trunk,
            "allowed": reason.allowed(),
            "retry_when": reason.retry_when(),
            "next_step": reason.next(),
        });
    }
}

/// ROADMAP § 3: an operation that ended shows its terminal record's timing.
fn timing_of(terminal: &[Value], op_id: &str) -> Option<Value> {
    terminal
        .iter()
        .rev()
        .filter(|entry| entry["op_id"] == op_id)
        .find_map(crate::timing::from_terminal)
}

/// What the record of `op` is: `published`, the id of its newest receipt (synced or in
/// `unsynced`), an outcome in the chain, or `missing`.
fn receipt_state(
    record: &ChangeRecord,
    unsynced: &[&Receipt],
    op: &crate::change::Operation,
    published: bool,
) -> Value {
    let receipt = record
        .receipts
        .iter()
        .chain(unsynced.iter().copied())
        .rev()
        .find(|receipt| receipt.op_id.as_deref() == Some(op.op_id.as_str()))
        .map(|receipt| receipt.id.clone());
    // Never success without proof: an unpublished operation with no outcome and no receipt
    // has a missing record.
    match (&receipt, published, &op.outcome) {
        (_, true, _) => json!("merged"),
        (Some(id), false, _) => json!(id),
        (None, false, Some(_)) => json!("outcome recorded in the chain"),
        (None, false, None) => json!("missing"),
    }
}

fn describe(
    repo: &Repo,
    record: &ChangeRecord,
    trunk: &str,
    unsynced: &[&Receipt],
    read: &Read,
) -> Result<Value, String> {
    let found = scan(&repo.git, &record.base_first, trunk, &record.id)?;
    let landed = found.publications.first();
    let ((op_cost, landing_cost), probe) = match read {
        Read::Full { registry, .. } => {
            (costs(repo, record, landed, registry.as_ref())?, Probe::Run)
        }
        Read::Cached => ((json!(null), json!(null)), Probe::Cached),
    };
    let judged = live_inputs(repo, record, landed.is_some(), trunk, probe)?;
    let state = if landed.is_some() {
        "landed".to_owned()
    } else {
        serde_json::to_value(crate::inputs::derived_state(&judged).unwrap_or(record.state))
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    let terminal = crate::log::records_for(&repo.git, &record.id)?;
    let mut operation = record.operations.last().map(|op| {
        let published = landed.is_some_and(|found| found.revision == op.revision);
        let record_state = receipt_state(record, unsynced, op, published);
        let reason = op.outcome.as_deref().and_then(Reason::from_code);
        let lease = lease(repo, record, op, published);
        let mut operation = json!({
            "lease": lease,
            "owner": op.owner,
            "receipt": record_state,
            "allowed": reason.map(Reason::allowed),
            "retry_when": reason.map(Reason::retry_when),
            "next_step": reason.map(Reason::next),
            "op_id": op.op_id,
            "state": if landed.is_some_and(|found| found.revision == op.revision) { json!("published") } else { json!(op.state) },
            "outcome": op.outcome,
            "attempts": op.attempts.len(),
            "trunk_moved": op.trunk_moved,
            "policy_changed": op.policy_changed,
            "publish_retries": op.publish_retries,
            "candidate": op.attempts.last().map(|attempt| &attempt.candidate_id),
            "published": landed.is_some_and(|found| found.revision == op.revision),
            "cost": op_cost,
        });
        // G80: an operation under way says how long it has been and what comes next.
        if !published && !op.state.is_terminal() {
            let (elapsed, _, next) = crate::publish::progress(&repo.git, op);
            operation["elapsed_s"] = json!(elapsed);
            operation["next"] = json!(next);
        }
        if !published && !op.state.is_terminal() && op.state != crate::change::OpState::Blocked {
            waiting(op, &mut operation);
        }
        if let Some(timing) = timing_of(&terminal, &op.op_id) {
            operation["timing"] = timing;
        }
        operation
    });
    // G245: a change held for an approval names who can give it and the candidate to name.
    if let (Some(operation), Some(approval)) =
        (operation.as_mut(), approval_of(repo, record, read)?)
    {
        operation["approval"] = approval;
    }
    Ok(json!({
        "change": record.id,
        "intent": record.intent.reference,
        "lane": record.lane,
        "from": record.from,
        "state": state,
        "recorded_state": record.state,
        "landed": landed.map(|found| &found.commit),
        "landing_cost": landing_cost,
        "inputs": crate::inputs::to_json(judged.iter()),
        "invalidations": crate::inputs::to_json(judged.iter().filter(|judgement| judgement.is_stale())),
        "validated_candidate": record.validated_candidate,
        "operation": operation,
        "anomalies": found.anomalies.iter().map(|anomaly| json!({
            "commit": anomaly.commit,
            "why": "carries the Change-Id but is not a verified merge",
        })).collect::<Vec<_>>(),
        "completion": completion(repo, record, trunk, landed, read)?,
        "evidence": evidence_read(repo, record, landed, read)?,
        "post_checks": post_checks(repo, landed, read),
        "retention_cut": record.retention_cut,
        "terminal": terminal,
        "receipts": record.receipts.iter().filter(|receipt| {
            // Synced means on the remote: a receipt whose pending file remains is not, even
            // when the local chain already holds it.
            !unsynced.iter().any(|pending| pending.id == receipt.id)
        }).map(|receipt| json!({
            "id": receipt.id, "op_id": receipt.op_id, "step": receipt.step, "reason": receipt.reason,
            "recovery": receipt.recovery, "at": receipt.at, "synced": true,
        })).chain(unsynced.iter().map(|receipt| json!({
            "id": receipt.id, "op_id": receipt.op_id, "step": receipt.step, "reason": receipt.reason,
            "recovery": receipt.recovery, "at": receipt.at, "age_s": receipts::age_s(receipt), "synced": false,
        }))).collect::<Vec<_>>(),
    }))
}

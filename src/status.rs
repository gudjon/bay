//! `bay status`: current state, derived on read (`DESIGN` § 4, `INTERFACES` § 1).
//!
//! "Landed" comes from trunk, never from the chain alone: a change whose publication is on
//! trunk is landed even when its chain was not updated before a crash. Built here: one change
//! or every local change, with freshness. Leftovers (G41) and the fleet map are later items;
//! `--fleet` builds only its fail-closed path (G49).

use std::path::Path;

use serde_json::{Value, json};

use crate::change::{CHANGES_PREFIX, Chain, ChangeRecord, change_ref};
use crate::clock::now_rfc3339;
use crate::evidence::{self, EvidenceRecord};
use crate::publish::scan;
use crate::receipts::{self, Receipt};
use crate::repo::Repo;
use crate::result::{BayResult, Freshness, Reason};
use crate::view;

#[must_use]
pub fn status(dir: &Path, change: Option<&str>) -> BayResult {
    run(dir, change).unwrap_or_else(BayResult::failed)
}

/// `bay status --fleet`: the fleet map read from the local cache (`DESIGN` § 11a,
/// `INTERFACES` § 5a). It fetches nothing, opens no SSH to find machines and starts no session.
///
/// Built here: the fail-closed path (G49). A cache with no fetch of the configured remote is
/// not the fleet, so the result is `blocked` with `REMOTE_NOT_FETCHED`, and its detail lists
/// no machines, actors, talks or sessions. The map itself reads `refs/bay/live/<machine>`,
/// which increment 2 brings; until then a fetched cache is an operation fault that says so,
/// never an empty map presented as the fleet.
#[must_use]
pub fn fleet(dir: &Path) -> BayResult {
    run_fleet(dir).unwrap_or_else(BayResult::failed)
}

fn run_fleet(dir: &Path) -> Result<BayResult, String> {
    let repo = Repo::open(dir)?;
    let remote = repo.remote();
    let url = repo
        .git
        .remote_url(remote)
        .map_err(|error| error.message())?;
    let configured = url.is_some();
    let fetched = match &url {
        Some(url) => fleet_fetch_recorded(&repo, remote, url)?,
        None => false,
    };
    if !fetched {
        return Ok(
            BayResult::blocked(Reason::RemoteNotFetched).with_detail(json!({
                "remote": remote,
                "remote_configured": configured,
                "fetched": false,
                "note": "the local cache has no fetch of the configured remote, so it is not the fleet",
            })),
        );
    }
    Err(format!(
        "bay status --fleet: the remote {remote} has been fetched, but this bay version does not \
         build the fleet map (refs/bay/live, increment 2); only the fail-closed path is built"
    ))
}

/// Where bay records a fetch of the remote's fleet refs (`refs/bay/live/*`): a local file
/// beside the views, never pushed (ruling: what "fetched" means for `--fleet`).
fn fleet_fetch_record(repo: &Repo) -> std::path::PathBuf {
    repo.common_dir
        .join("bay")
        .join("fleet")
        .join("fetched.json")
}

/// Whether this cache holds a fetch of `remote`'s fleet refs: the record a fetch of
/// `refs/bay/live/*` writes, naming the remote and the URL it was fetched from. A trunk
/// tracking ref is not that fetch: a plain push writes it, and bay's trunk fetch brings no
/// `refs/bay/live`. A record for another URL is a fetch of another remote. No command in this
/// version writes the record (the live refs are increment 2), so `--fleet` fails closed.
fn fleet_fetch_recorded(repo: &Repo, remote: &str, url: &str) -> Result<bool, String> {
    let path = fleet_fetch_record(repo);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    // An unreadable record is no proof of a fetch: fail closed.
    let Ok(record) = serde_json::from_str::<Value>(&text) else {
        return Ok(false);
    };
    Ok(record["remote"] == remote && record["url"] == url && record["at"].is_string())
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
    let chain = Chain::new(&repo.git);
    let ids = repo
        .git
        .run(&["for-each-ref", "--format=%(refname)", CHANGES_PREFIX])
        .map_err(|error| error.message())?;
    let mut changes = Vec::new();
    for id in ids
        .lines()
        .map(|name| name.trim_start_matches(CHANGES_PREFIX))
    {
        let Ok((_, record)) = chain.read(id) else {
            continue;
        };
        if !select(id, &record) {
            continue;
        }
        let unsynced: Vec<&Receipt> = pending
            .iter()
            .map(|(_, receipt)| receipt)
            .filter(|receipt| receipt.change == id)
            .collect();
        changes.push(describe(repo, &record, &trunk, &unsynced, &Costs::Skip)?);
    }
    let mut result = BayResult::ok(json!({"changes": changes, "trunk": trunk}));
    result.freshness = Some(Freshness {
        trunk,
        fetched_at: CACHED.to_owned(),
    });
    Ok(result)
}

fn run(dir: &Path, change: Option<&str>) -> Result<BayResult, String> {
    let repo = Repo::open(dir)?;
    let _ = receipts::sync(&repo);
    let pending = receipts::pending(&repo);
    let (trunk, fetched_at) = match repo.fetch_trunk() {
        Ok(trunk) => (trunk, Some(now_rfc3339())),
        Err(_) => (
            repo.git
                .run(&["rev-parse", "--verify", &repo.trunk_tracking_ref()])
                .map_err(|error| error.message())?,
            None,
        ),
    };
    let ids: Vec<String> = match change.map(str::to_owned).or_else(|| repo.view_change(dir)) {
        Some(id) => vec![id],
        None => repo
            .git
            .run(&["for-each-ref", "--format=%(refname)", CHANGES_PREFIX])
            .map_err(|error| error.message())?
            .lines()
            .map(|name| name.trim_start_matches(CHANGES_PREFIX).to_owned())
            .collect(),
    };
    let chain = Chain::new(&repo.git);
    crate::log::fetch(&repo);
    for id in &ids {
        // The chain is read fresh: a fast-forward from the remote brings what an executor
        // recorded elsewhere. Local work the remote lacks was pushed by the sync above.
        let name = change_ref(id);
        if repo
            .git
            .run(&[
                "fetch",
                "-q",
                "--no-tags",
                repo.remote(),
                &format!("{name}:{name}"),
            ])
            .is_err()
        {
            // Not a fast-forward: the one rewrite of a chain is the retention cut, which
            // replaces the local chain. The remote chain is fetched into a ref of its own, never
            // read from FETCH_HEAD: a fetch that fails can leave FETCH_HEAD holding another
            // change's chain.
            let fetched_ref = format!("refs/bay/fetch/{id}");
            if repo
                .git
                .run(&[
                    "fetch",
                    "-q",
                    "--no-tags",
                    repo.remote(),
                    &format!("+{name}:{fetched_ref}"),
                ])
                .is_ok()
                && let Ok(fetched) = repo.git.run(&["rev-parse", "--verify", &fetched_ref])
                && chain
                    .record_at(&fetched)
                    .is_ok_and(|record| record.retention_cut && record.id == *id)
            {
                let _ = repo.git.run(&["update-ref", &name, &fetched]);
            }
            let _ = repo.git.run(&["update-ref", "-d", &fetched_ref]);
        }
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
    let under = Costs::Under(crate::registry::Registry::current(&repo.git)?);
    let mut changes = Vec::new();
    for id in &ids {
        let (_, record) = chain.read(id).map_err(|error| error.message())?;
        let unsynced: Vec<&Receipt> = pending
            .iter()
            .map(|(_, receipt)| receipt)
            .filter(|receipt| receipt.change == *id)
            .collect();
        changes.push(describe(&repo, &record, &trunk, &unsynced, &under)?);
    }
    let leftovers = leftovers(&repo, &trunk)?;
    let _ = &leftovers;
    let quarantines = quarantines(&repo, &trunk);
    let mut result = BayResult::ok(json!({
        "changes": changes,
        "leftovers": leftovers,
        "quarantines": quarantines,
        "trunk": trunk,
    }));
    if let [only] = ids.as_slice() {
        result = result.with_change(only.clone());
    }
    result.freshness = Some(Freshness {
        trunk,
        fetched_at: fetched_at.unwrap_or_else(|| "not fetched (remote unreachable)".to_owned()),
    });
    Ok(result)
}

/// The open quarantines of the policy at trunk (ruling 8c), so a set-aside check is never
/// silent. One past its end date stays in force and is shown as `overdue`. An unreadable
/// policy lists none here; `land` names it with `POLICY_INVALID`.
fn quarantines(repo: &Repo, trunk: &str) -> Vec<Value> {
    open_quarantines(&repo.git, trunk)
}

/// The open quarantines of the policy at `at`, each with whether it is overdue today. Read
/// by `status` and by `bay doctor` (`DESIGN` § 7).
pub(crate) fn open_quarantines(git: &crate::git::Git, at: &str) -> Vec<Value> {
    let Ok(Some(policy)) = crate::policy::load_at(git, at) else {
        return Vec::new();
    };
    let now = now_rfc3339();
    let today = now.get(..10).unwrap_or_default();
    policy
        .quarantined
        .iter()
        .map(|quarantine| {
            json!({
                "check": quarantine.id,
                "environment": quarantine.environment,
                "reason": quarantine.reason,
                "end": quarantine.end,
                "overdue": quarantine.overdue(today),
            })
        })
        .collect()
}

/// Every view this machine still holds, with its change state, its age and whether it has
/// uncheckpointed edits (G41). No pull-request state, no chat activity.
fn leftovers(repo: &Repo, trunk: &str) -> Result<Vec<Value>, String> {
    let chain = Chain::new(&repo.git);
    let mut out = Vec::new();
    for id in view::all(repo) {
        let Ok((_, record)) = chain.read(&id) else {
            continue;
        };
        let landed = !scan(&repo.git, &record.base_first, trunk, &id)?
            .publications
            .is_empty();
        let state = if landed {
            json!("landed")
        } else {
            json!(record.state)
        };
        let path = repo.view_path(&id);
        let latest = chain.drafts(&id).map_err(|error| error.message())?[0].clone();
        let uncheckpointed = if view::exists(&path) {
            json!(view::uncheckpointed(&path, &latest).map_err(|error| error.message())?)
        } else {
            json!(null)
        };
        // § 9a: after a landing, the view is releasable only when it holds no newer work than
        // the published revision. Newer work is kept, never cleaned away.
        let mut newer_work = false;
        if landed && view::exists(&path) {
            let published = record
                .operations
                .iter()
                .rev()
                .find(|op| op.state == crate::change::OpState::Published)
                .map_or_else(|| latest.clone(), |op| op.revision.clone());
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

/// Whether `describe` reads the evidence behind the costs, and the registry it counts it under.
enum Costs {
    /// The hook's path reads no evidence: its costs are `null`.
    Skip,
    Under(Option<crate::registry::Registry>),
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

fn describe(
    repo: &Repo,
    record: &ChangeRecord,
    trunk: &str,
    unsynced: &[&Receipt],
    with_costs: &Costs,
) -> Result<Value, String> {
    let found = scan(&repo.git, &record.base_first, trunk, &record.id)?;
    let landed = found.publications.first();
    let (op_cost, landing_cost) = match with_costs {
        Costs::Under(registry) => costs(repo, record, landed, registry.as_ref())?,
        Costs::Skip => (json!(null), json!(null)),
    };
    let state = if landed.is_some() {
        "landed".to_owned()
    } else {
        serde_json::to_value(record.state)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    let operation = record.operations.last().map(|op| {
        let published = landed.is_some_and(|found| found.revision == op.revision);
        let receipt = record
            .receipts
            .iter()
            .chain(unsynced.iter().copied())
            .rev()
            .find(|receipt| receipt.op_id.as_deref() == Some(op.op_id.as_str()))
            .map(|receipt| receipt.id.clone());
        // Never success without proof: an unpublished operation with no outcome and no receipt
        // has a missing record.
        let record_state = match (&receipt, published, &op.outcome) {
            (_, true, _) => json!("published"),
            (Some(id), false, _) => json!(id),
            (None, false, Some(_)) => json!("outcome recorded in the chain"),
            (None, false, None) => json!("missing"),
        };
        let reason = op.outcome.as_deref().and_then(Reason::from_code);
        // A one-off status cannot watch a lease; it reports a suspected expiry from the age of
        // the chain head and never claims (§ 9a).
        let lease = if op.owner.is_some()
            && matches!(op.state, crate::change::OpState::Running | crate::change::OpState::Checked)
            && !published
        {
            let age = repo
                .git
                .run(&["log", "-1", "--format=%ct", &crate::change::change_ref(&record.id)])
                .ok()
                .and_then(|time| time.trim().parse::<u64>().ok())
                .map(|time| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |now| now.as_secs())
                        .saturating_sub(time)
                });
            json!(if age.is_some_and(|age| age >= repo.config.lease().as_secs()) { "suspected expired" } else { "held" })
        } else {
            json!(null)
        };
        json!({
            "lease": lease,
            "owner": op.owner,
            "receipt": record_state,
            "allowed": reason.map(Reason::allowed),
            "retry_when": reason.map(Reason::retry_when),
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
        })
    });
    Ok(json!({
        "change": record.id,
        "intent": record.intent.reference,
        "state": state,
        "recorded_state": record.state,
        "landed": landed.map(|found| &found.commit),
        "landing_cost": landing_cost,
        "operation": operation,
        "anomalies": found.anomalies.iter().map(|anomaly| json!({
            "commit": anomaly.commit,
            "why": "carries the Change-Id but is not a verified publication",
        })).collect::<Vec<_>>(),
        // § 7: the completion dimensions are reported apart, never collapsed into "done".
        "completion": {
            "published": landed.is_some(),
            "checked": landed.is_some() || record.operations.last().is_some_and(|op| op.attempts.last().is_some_and(|attempt| attempt.verdict == "pass")),
            "running": "unconfirmed",
            "effective": "unconfirmed",
        },
        "retention_cut": record.retention_cut,
        "terminal": crate::log::records_for(&repo.git, &record.id)?,
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

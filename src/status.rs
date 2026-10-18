//! `bay status`: current state, derived on read (`DESIGN` § 4, `INTERFACES` § 1).
//!
//! "Landed" comes from trunk, never from the chain alone: a change whose publication is on
//! trunk is landed even when its chain was not updated before a crash. Built here: one change
//! or every local change, with freshness. Leftovers (G41) and the fleet map are later items.

use std::path::Path;

use serde_json::{Value, json};

use crate::change::{CHANGES_PREFIX, Chain, ChangeRecord, change_ref};
use crate::clock::now_rfc3339;
use crate::publish::scan;
use crate::receipts::{self, Receipt};
use crate::repo::Repo;
use crate::result::{BayResult, Freshness, Reason};
use crate::view;

#[must_use]
pub fn status(dir: &Path, change: Option<&str>) -> BayResult {
    run(dir, change).unwrap_or_else(BayResult::failed)
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
    let mut changes = Vec::new();
    for id in &ids {
        let (_, record) = chain.read(id).map_err(|error| error.message())?;
        let unsynced: Vec<&Receipt> = pending
            .iter()
            .map(|(_, receipt)| receipt)
            .filter(|receipt| receipt.change == *id)
            .collect();
        changes.push(describe(&repo, &record, &trunk, &unsynced)?);
    }
    let leftovers = leftovers(&repo, &trunk)?;
    let _ = &leftovers;
    let mut result =
        BayResult::ok(json!({"changes": changes, "leftovers": leftovers, "trunk": trunk}));
    if let [only] = ids.as_slice() {
        result = result.with_change(only.clone());
    }
    result.freshness = Some(Freshness {
        trunk,
        fetched_at: fetched_at.unwrap_or_else(|| "not fetched (remote unreachable)".to_owned()),
    });
    Ok(result)
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

fn describe(
    repo: &Repo,
    record: &ChangeRecord,
    trunk: &str,
    unsynced: &[&Receipt],
) -> Result<Value, String> {
    let found = scan(&repo.git, &record.base_first, trunk, &record.id)?;
    let landed = found.publications.first();
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
        })
    });
    Ok(json!({
        "change": record.id,
        "intent": record.intent.reference,
        "state": state,
        "recorded_state": record.state,
        "landed": landed.map(|found| &found.commit),
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

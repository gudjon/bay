//! `bay waiting` and `bay approve <change>` (G280, G281; owner ruling 2026-10-21): one command
//! to see what waits for a person's yes, and one to give it, for people who never learn Git.
//!
//! - **`bay waiting`** lists the changes whose last operation is held for an approval this
//!   machine's approver can give: `bay.approverName` is set, and that name is an enrolled
//!   approver of a pending approval check. With no approver set it lists every change that
//!   waits for any approval, and who can approve each. An item is what `bay status` names under
//!   `operation.approval` ([`crate::status::approval_pending`]), with the change's intent and
//!   actor, the changed paths under each check's `when_paths`, and a one-line summary of the
//!   candidate's diff. `detail.waiting` is the list; the decision is `ok` either way.
//! - **`bay approve <change>`** records this machine's approver's signed `approval` record,
//!   `pass`, or `product_failure` with `--reject`, for every pending approval check of that
//!   change the approver is enrolled for, on the exact candidate commit its pending operation
//!   names, found as `bay waiting` finds it. It writes through `bay evidence record`
//!   ([`crate::outside`]), so signing, the key class and the push are that path's. `--why` is
//!   the record's `why`.
//! - **N-9 waivers.** `bay waiting` also lists each change blocked on a check the policy at its
//!   candidate's base marks `waivable = true` that did not pass, with its verdict and the
//!   command that waives it, when the registry enrols this machine's approver (or none is
//!   set). `bay approve <change> --waive <check> --why <text>` records one signed `waiver` of
//!   that one check on that exact candidate, with the reason: never a pass, and never carried
//!   to another candidate. The landing names it under `coverage_gaps` with the real verdict.
//!
//! Neither ever answers for another name: the record's writer is `bay.approverName`, signed by
//! `bay.approverKey`, and there is no flag that names someone else. Every refusal names its
//! reason and the recovery.
//!
//! The chains are read as the remote holds them (a land adopts the remote's chain too), copied
//! under [`crate::names::FETCH_WAITING`], never into this machine's own change refs; a change
//! the remote lacks is read from this machine's.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::{Value, json};

use crate::change::{CHANGES_PREFIX, Chain, ChangeRecord, change_ref};
use crate::evidence;
use crate::git::Git;
use crate::names::FETCH_WAITING;
use crate::outside;
use crate::registry::Registry;
use crate::repo::Repo;
use crate::result::{BayResult, Decision, Reason};

/// What `bay approve` asks.
pub struct Approve {
    /// The change that waits for the approval.
    pub change: String,
    /// Say no: the verdict is `product_failure`.
    pub reject: bool,
    /// N-9: waive this one check, marked `waivable = true`, on the pending candidate, instead
    /// of approving: a signed `waiver` record, never a pass.
    pub waive: Option<String>,
    /// The record's `why`; a waiver needs one.
    pub why: Option<String>,
}

/// `bay waiting`.
#[must_use]
pub fn waiting(dir: &Path) -> BayResult {
    let repo = match Repo::open_fetched(dir) {
        Ok(repo) => repo,
        Err(error) => return error.into_result(),
    };
    let mut result = waiting_in(&repo).unwrap_or_else(BayResult::failed);
    result.freshness = result.freshness.or_else(|| repo.freshness());
    result
}

/// `bay approve <change>`.
#[must_use]
pub fn approve(dir: &Path, request: &Approve) -> BayResult {
    let repo = match Repo::open_fetched(dir) {
        Ok(repo) => repo,
        Err(error) => return error.into_result(),
    };
    let mut result = approve_in(dir, &repo, request).unwrap_or_else(BayResult::failed);
    result.freshness = result.freshness.or_else(|| repo.freshness());
    result
}

/// One change whose last operation is held for an approval.
struct Held {
    record: ChangeRecord,
    op_id: String,
    base: String,
    commit: String,
    tree: String,
    /// `bay status`'s `operation.approval`: `checks`, `commit`, `tree`, and `why` without a
    /// registry.
    approval: Value,
}

impl Held {
    /// The approval checks still owed (`state` `pending`), narrowed to those `approver` is
    /// enrolled for when one is named.
    fn pending(&self, approver: Option<&str>) -> Vec<Value> {
        self.approval["checks"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|row| row["state"] == "pending")
                    .filter(|row| approver.is_none_or(|name| may_approve(row, name)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Whether `name` is an enrolled approver of the check `row` names.
fn may_approve(row: &Value, name: &str) -> bool {
    row["approvers"]
        .as_array()
        .is_some_and(|names| names.iter().any(|enrolled| enrolled == name))
}

/// This machine's approver: `bay.approverName` and `bay.approverKey`, each when set.
fn approver(git: &Git) -> (Option<String>, Option<String>) {
    let get = |key: &str| {
        git.config_get(key)
            .ok()
            .flatten()
            .filter(|value| !value.trim().is_empty())
    };
    (get("bay.approverName"), get("bay.approverKey"))
}

/// Brings trunk, the registry and the remote's evidence here, so an approval another approver
/// already gave is seen, and who may approve is read from the remote's registry.
fn synchronize(repo: &Repo) -> Result<(), String> {
    repo.fetch_trunk().map_err(|error| error.message())?;
    repo.fetch_registry();
    repo.fetch_evidence(&crate::signing::Keys::load(&repo.git));
    Ok(())
}

/// The change records to judge, as the remote holds them: every change, or only `only`. A
/// change the remote lacks is read from this machine's own chain.
fn records(repo: &Repo, only: Option<&str>) -> Result<Vec<ChangeRecord>, String> {
    let git = &repo.git;
    let refspec = only.map_or_else(
        || format!("+{CHANGES_PREFIX}*:{FETCH_WAITING}*"),
        |id| format!("+{}:{FETCH_WAITING}{id}", change_ref(id)),
    );
    match only {
        // Every chain: the copy is pruned to the remote's, so a chain gone there is gone here.
        None => {
            git.run(&[
                "fetch",
                "-q",
                "--no-tags",
                "--prune",
                repo.remote(),
                &refspec,
            ])
            .map_err(|error| {
                format!(
                    "the changes on {} could not be read: {}",
                    repo.remote(),
                    error.message()
                )
            })?;
        }
        // One chain: a change the remote lacks is no fault; this machine's chain is read.
        Some(id) => {
            if git
                .run(&["fetch", "-q", "--no-tags", repo.remote(), &refspec])
                .is_err()
            {
                let _ = git.run(&["update-ref", "-d", &format!("{FETCH_WAITING}{id}")]);
            }
        }
    }
    let chain = Chain::new(git);
    let pattern = only.map_or_else(
        || FETCH_WAITING.to_owned(),
        |id| format!("{FETCH_WAITING}{id}"),
    );
    let listed = git
        .run(&[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            &pattern,
        ])
        .map_err(|error| error.message())?;
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for line in listed.lines() {
        let Some((head, name)) = line.split_once(' ') else {
            continue;
        };
        let id = name.trim_start_matches(FETCH_WAITING);
        if only.is_some_and(|only| only != id) {
            continue;
        }
        if let Ok(record) = chain.record_at(head) {
            seen.insert(id.to_owned());
            out.push(record);
        }
    }
    let local: Vec<String> = match only {
        Some(id) => vec![id.to_owned()],
        None => git
            .run(&["for-each-ref", "--format=%(refname)", CHANGES_PREFIX])
            .map_err(|error| error.message())?
            .lines()
            .map(|name| name.trim_start_matches(CHANGES_PREFIX).to_owned())
            .collect(),
    };
    for id in local.iter().filter(|id| !seen.contains(*id)) {
        if let Ok((_, record)) = chain.read(id) {
            out.push(record);
        }
    }
    Ok(out)
}

/// `record` when its last operation is held for an approval, read as `bay status` reads it.
///
/// # Errors
///
/// The approval cannot be read (a git fault, an unreadable policy or note).
fn held(
    repo: &Repo,
    record: &ChangeRecord,
    registry: Option<&Registry>,
) -> Result<Option<Held>, String> {
    let Some(approval) = crate::status::approval_pending(repo, record, registry)? else {
        return Ok(None);
    };
    if let Some(why) = approval.get("unread") {
        return Err(format!(
            "the approval change {} waits on cannot be read: {}",
            record.id,
            why.as_str().unwrap_or_default()
        ));
    }
    let Some(op) = record.operations.last() else {
        return Ok(None);
    };
    let Some(attempt) = op.attempts.last() else {
        return Ok(None);
    };
    Ok(Some(Held {
        record: record.clone(),
        op_id: op.op_id.clone(),
        base: attempt.base.clone(),
        commit: attempt.commit.clone(),
        tree: attempt.tree.clone(),
        approval,
    }))
}

/// The paths of `rows`, each once, in order.
fn paths_of(rows: &[Value]) -> Vec<String> {
    let mut paths = BTreeSet::new();
    for row in rows {
        for path in row["paths"].as_array().into_iter().flatten() {
            if let Some(path) = path.as_str() {
                paths.insert(path.to_owned());
            }
        }
    }
    paths.into_iter().collect()
}

/// The candidate's diff against the trunk it was merged onto, in one line: files added,
/// changed and removed, and lines added and removed (a binary file counts no lines).
fn diff_summary(git: &Git, base: &str, commit: &str) -> Result<Value, String> {
    let run = |format: &str| {
        git.run(&["diff-tree", "-r", "--no-renames", format, base, commit])
            .map_err(|error| error.message())
    };
    let (mut added, mut changed, mut removed) = (0_u64, 0_u64, 0_u64);
    for line in run("--name-status")?.lines() {
        match line.chars().next() {
            Some('A') => added += 1,
            Some('D') => removed += 1,
            Some(_) => changed += 1,
            None => {}
        }
    }
    let (mut plus, mut minus) = (0_u64, 0_u64);
    for line in run("--numstat")?.lines() {
        let mut columns = line.split('\t');
        plus += columns
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);
        minus += columns
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);
    }
    let files = added + changed + removed;
    let noun = if files == 1 { "file" } else { "files" };
    Ok(json!({
        "files": files,
        "added": added,
        "changed": changed,
        "removed": removed,
        "lines_added": plus,
        "lines_removed": minus,
        "summary": format!(
            "{files} {noun}: {added} added, {changed} changed, {removed} removed; +{plus} -{minus} lines"
        ),
    }))
}

/// One item of `detail.waiting`: the change, its intent and actor, the approval checks `rows`
/// with who can approve each and the paths each covers, the candidate, and its diff.
fn item(git: &Git, held: &Held, rows: &[Value]) -> Result<Value, String> {
    let checks: Vec<&Value> = rows.iter().map(|row| &row["check"]).collect();
    let mut approvers = BTreeSet::new();
    for row in rows {
        for name in row["approvers"].as_array().into_iter().flatten() {
            if let Some(name) = name.as_str() {
                approvers.insert(name.to_owned());
            }
        }
    }
    let mut item = json!({
        "change": held.record.id,
        "intent": held.record.intent.reference,
        "actor": held.record.actor.name,
        "op_id": held.op_id,
        "checks": checks,
        "approvals": rows,
        "approvers": approvers,
        "commit": held.commit,
        "tree": held.tree,
        "paths": paths_of(rows),
        "diff": diff_summary(git, &held.base, &held.commit)?,
        "approve": format!("bay approve {}", held.record.id),
        "reject": format!("bay reject {} --why <text>", held.record.id),
    });
    if let Some(why) = held.approval.get("why") {
        item["why"] = why.clone();
    }
    Ok(item)
}

fn waiting_in(repo: &Repo) -> Result<BayResult, String> {
    let (name, _) = approver(&repo.git);
    synchronize(repo)?;
    let registry = Registry::current(&repo.git)?;
    let mut items = Vec::new();
    let mut unread = Vec::new();
    let mut others = 0_usize;
    // N-9: a waiver counts only from an approver the registry enrols, so without a registry,
    // or for a name it does not enrol, no waivable check is listed.
    let waives = registry
        .as_ref()
        .is_some_and(|registry| name.as_deref().is_none_or(|name| enrolled(registry, name)));
    for record in records(repo, None)? {
        let found = match held(repo, &record, registry.as_ref()) {
            Ok(Some(found)) => found,
            Ok(None) => {
                match waivable(repo, &record, registry.as_ref()) {
                    Ok(Some(found)) if waives && !found.rows.is_empty() => {
                        items.push(waiver_item(&repo.git, &found)?);
                    }
                    Ok(_) => {}
                    Err(why) => unread.push(json!({"change": record.id, "why": why})),
                }
                continue;
            }
            Err(why) => {
                unread.push(json!({"change": record.id, "why": why}));
                continue;
            }
        };
        // G280: with an approver set, only what that name may give; never another's.
        let rows = found.pending(name.as_deref());
        if rows.is_empty() {
            others += usize::from(!found.pending(None).is_empty());
            continue;
        }
        items.push(item(&repo.git, &found, &rows)?);
    }
    let mut detail = json!({"approver": name, "waiting": items});
    if name.is_some() && others > 0 {
        detail["others"] = json!(others);
        detail["note"] = json!(format!(
            "{others} other change(s) wait for an approval this approver is not enrolled for; bay waiting with no bay.approverName set lists them, with who can approve each"
        ));
    }
    if !unread.is_empty() {
        detail["unread"] = Value::from(unread);
    }
    Ok(BayResult::ok(detail))
}

/// A refusal of `bay approve`: why, what to do, and `more` beside them.
fn refused(change: &str, why: &str, recovery: &str, more: Option<Value>) -> BayResult {
    let mut detail = json!({"why": why, "recovery": recovery});
    if let (Some(Value::Object(more)), Some(object)) = (more, detail.as_object_mut()) {
        object.extend(more);
    }
    BayResult::new(Decision::Failed)
        .with_change(change.to_owned())
        .with_detail(detail)
}

/// What every refusal of a change that waits on nothing points to.
const READ_WAITING: &str = "bay waiting lists the changes that wait for an approval";

/// Why `record` waits on no approval now: each approval check of its held candidate is
/// answered already (`held`), a newer operation replaced the one held for an approval (the
/// candidate moved), or none was ever held.
fn nothing_pending(record: &ChangeRecord, held: Option<&Held>) -> BayResult {
    let id = &record.id;
    if let Some(held) = held {
        return refused(
            id,
            &format!(
                "change {id} waits on no approval anyone can still give: each approval check of candidate {} is answered",
                held.commit
            ),
            READ_WAITING,
            Some(json!({"approval": held.approval["checks"]})),
        );
    }
    let Some(op) = record.operations.last() else {
        return refused(
            id,
            &format!("change {id} waits on no approval: it was never submitted"),
            READ_WAITING,
            None,
        );
    };
    let state = op.outcome.as_deref().unwrap_or(op_state(op));
    let earlier = record.operations.iter().rev().skip(1).any(|op| {
        op.outcome.as_deref().and_then(Reason::from_code) == Some(Reason::ApprovalPending)
    });
    if earlier {
        return refused(
            id,
            &format!(
                "the candidate moved: operation {} of change {id} is newer than the one held for an approval, and it is {state}",
                op.op_id
            ),
            &format!(
                "read it again: bay waiting names the new candidate once it is held {}; approve that one",
                Reason::ApprovalPending.code()
            ),
            None,
        );
    }
    refused(
        id,
        &format!(
            "change {id} waits on no approval: its last operation {} is {state}",
            op.op_id
        ),
        READ_WAITING,
        None,
    )
}

/// An operation's state as the chain spells it.
const fn op_state(op: &crate::change::Operation) -> &'static str {
    use crate::change::OpState;
    match op.state {
        OpState::Submitted => "submitted",
        OpState::Admitted => "admitted",
        OpState::Running => "running",
        OpState::Checked => "checked",
        OpState::Published => "published",
        OpState::Blocked => "blocked",
        OpState::Failed => "failed",
        OpState::Superseded => "superseded",
        OpState::NoEffect => "no_effect",
    }
}

/// What happens after the answer: who lands the change (G304, `lands_by`: `executor`, `here`,
/// `author`, or `author_repairs` after a no), and the step in words. A running executor lands
/// an approved change on its next pass (G284); without one, the change's author lands it.
fn next_step(repo: &Repo, held: &Held, reject: bool) -> (&'static str, String) {
    let actor = &held.record.actor.name;
    if reject {
        return (
            "author_repairs",
            format!(
                "the change is blocked: {actor}, its author, repairs it and lands it again, and the new candidate waits for an approval"
            ),
        );
    }
    let view = repo.view_path(&held.record.id);
    if view.is_dir() {
        return (
            "here",
            format!(
                "this machine holds the change's view ({}): bay merge --wait there lands it. bay approve runs nothing else",
                view.display()
            ),
        );
    }
    crate::fleet::fetch(repo);
    let live = matches!(
        crate::fleet::executors(repo),
        Ok(Some(rows)) if crate::presence::any_fresh(&rows)
    );
    if live {
        (
            "executor",
            "an executor runs: its next pass lands the change; nobody needs to run bay merge again"
                .to_owned(),
        )
    } else {
        (
            "author",
            format!("{actor}, the change's author, runs bay merge --wait, and the change lands"),
        )
    }
}

fn approve_in(dir: &Path, repo: &Repo, request: &Approve) -> Result<BayResult, String> {
    let id = request.change.as_str();
    // G281: the writer is this machine's approver, by its own name and key, and no one else.
    let (Some(name), Some(_)) = approver(&repo.git) else {
        return Ok(refused(
            id,
            "no approver key is configured on this machine: bay.approverName and bay.approverKey are not both set",
            "the person who approves sets them once, on their own machine, with git config bay.approverName <name> and git config bay.approverKey <key>; the registry's approver file enrols that name and key",
            None,
        ));
    };
    // N-9: a waiver is the same person's word through the same command, about one check.
    if let Some(check) = request.waive.as_deref() {
        return waive_in(dir, repo, request, &name, check);
    }
    synchronize(repo)?;
    let registry = Registry::current(&repo.git)?;
    let Some(record) = records(repo, Some(id))?.into_iter().next() else {
        return Ok(refused(
            id,
            &format!("change {id} is not on {} or on this machine", repo.remote()),
            READ_WAITING,
            None,
        ));
    };
    // G281: the candidate is the one `bay waiting` and `bay status` name.
    let Some(found) = held(repo, &record, registry.as_ref())? else {
        return Ok(nothing_pending(&record, None));
    };
    let owed = found.pending(None);
    if owed.is_empty() {
        return Ok(nothing_pending(&record, Some(&found)));
    }
    // G281: only the checks this name is enrolled for; never one another approver must give.
    let mine = found.pending(Some(name.as_str()));
    if mine.is_empty() {
        let who: Vec<Value> = owed
            .iter()
            .map(|row| json!({"check": row["check"], "approvers": row["approvers"]}))
            .collect();
        return Ok(refused(
            id,
            &format!(
                "{name} is not an enrolled approver of any approval check change {id} waits on"
            ),
            "one of the approvers named here approves it, or the registry's approver file enrols this name for the check",
            Some(json!({"approver": name, "approvals": who})),
        ));
    }
    let checks: Vec<String> = mine
        .iter()
        .filter_map(|row| row["check"].as_str().map(str::to_owned))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let verdict = if request.reject {
        "product_failure"
    } else {
        "pass"
    };
    let records = match write(dir, &found, &checks, verdict, request.why.as_deref()) {
        Ok(records) => records,
        Err(refusal) => return Ok((*refusal).with_change(id.to_owned())),
    };
    // The change is read again: an operation that replaced the candidate while the record was
    // written is named. The answer still holds for the new candidate while the files its check
    // covers are unchanged (G292); `bay waiting` shows whether the new one still waits.
    if let Some(now) = records_now(repo, id).filter(|now| moved(now, &found.op_id, &found.commit)) {
        let newer = now
            .operations
            .last()
            .map(|op| op.op_id.clone())
            .unwrap_or_default();
        return Ok(refused(
            id,
            &format!(
                "the candidate moved: {name}'s {verdict} is recorded on candidate {}, and change {id} has a newer operation {newer} since",
                found.commit
            ),
            "read it again with bay waiting: your answer carries to the new candidate while the files it covers are unchanged, and the new one waits only if they changed",
            Some(json!({"records": records, "commit": found.commit})),
        ));
    }
    let (lands_by, next) = next_step(repo, &found, request.reject);
    Ok(BayResult::ok(json!({
        "approver": name,
        "verdict": verdict,
        "checks": checks,
        "commit": found.commit,
        "tree": found.tree,
        "records": records,
        "why": request.why,
        "lands_by": lands_by,
        "next": next,
    }))
    .with_change(id.to_owned())
    .with_op(found.op_id.clone()))
}

/// One signed `approval` record per check of `checks`, with `verdict`, about the candidate of
/// `found`, through `bay evidence record`'s own path: the ids it wrote or found unchanged, or
/// that path's refusal.
fn write(
    dir: &Path,
    found: &Held,
    checks: &[String],
    verdict: &str,
    why: Option<&str>,
) -> Result<Vec<Value>, Box<BayResult>> {
    let mut records = Vec::new();
    for check in checks {
        let written = outside::record(
            dir,
            &outside::Record {
                kind: evidence::KIND_APPROVAL.to_owned(),
                verdict: verdict.to_owned(),
                commits: vec![found.commit.clone()],
                check: Some(check.clone()),
                environment: None,
                inputs: Vec::new(),
                body: None,
                version: None,
                why: why.map(str::to_owned),
            },
        );
        if written.decision != Decision::Ok {
            return Err(Box::new(written));
        }
        if let Some(ids) = written
            .detail
            .as_ref()
            .and_then(|detail| detail["records"].as_array())
        {
            records.extend(ids.iter().cloned());
        }
    }
    Ok(records)
}

/// Whether `now`'s last operation, or its candidate, is another than operation `op_id` and its
/// candidate `commit`, the ones that were held.
fn moved(now: &ChangeRecord, op_id: &str, commit: &str) -> bool {
    now.operations.last().is_some_and(|op| {
        op.op_id != op_id
            || op
                .attempts
                .last()
                .is_some_and(|attempt| attempt.commit != commit)
    })
}

/// N-9: one change whose last operation is blocked on a failed, flaky or unanswered check, as
/// `bay waiting` and `bay approve --waive` read it.
struct Waivable {
    record: ChangeRecord,
    op_id: String,
    base: String,
    commit: String,
    tree: String,
    /// The blocking reason's code.
    reason: String,
    /// The waivable checks of the candidate that do not pass and no waiver answers yet
    /// ([`crate::publish::waivable_gaps`]).
    rows: Vec<Value>,
}

/// N-9: `record` when its last operation is blocked `CHECK_FAILED`, `CHECK_FLAKY` or
/// `CHECK_UNKNOWN` on a candidate of its own (no batch's), with the waivable checks of that
/// candidate that do not pass, judged by the policy at its base as its land judged it.
///
/// # Errors
///
/// A git fault, an unreadable policy or note, or a class file that cannot be written.
fn waivable(
    repo: &Repo,
    record: &ChangeRecord,
    registry: Option<&Registry>,
) -> Result<Option<Waivable>, String> {
    let Some(op) = record.operations.last() else {
        return Ok(None);
    };
    let reason = op.outcome.as_deref().and_then(Reason::from_code);
    let blocked = op.state == crate::change::OpState::Blocked
        && matches!(
            reason,
            Some(Reason::CheckFailed | Reason::CheckFlaky | Reason::CheckUnknown)
        );
    let Some(attempt) = op
        .attempts
        .last()
        .filter(|attempt| blocked && attempt.batch.is_none())
    else {
        return Ok(None);
    };
    let rows = crate::publish::waivable_gaps(
        &repo.git,
        (
            attempt.base.as_str(),
            attempt.commit.as_str(),
            attempt.tree.as_str(),
        ),
        registry,
    )?;
    Ok(Some(Waivable {
        record: record.clone(),
        op_id: op.op_id.clone(),
        base: attempt.base.clone(),
        commit: attempt.commit.clone(),
        tree: attempt.tree.clone(),
        reason: op.outcome.clone().unwrap_or_default(),
        rows,
    }))
}

/// N-9: the one command that waives `check` of change `id`.
fn waive_command(id: &str, check: &str) -> String {
    format!("bay approve {id} --waive {check} --why <text>")
}

/// N-9: one item of `detail.waiting` for a change held on checks a waiver may answer: the
/// change, its intent and actor, each waivable check that did not pass with its verdict and
/// the command that waives it, the candidate, and its diff.
fn waiver_item(git: &Git, found: &Waivable) -> Result<Value, String> {
    let id = &found.record.id;
    let rows: Vec<Value> = found
        .rows
        .iter()
        .map(|row| {
            let mut row = row.clone();
            let check = row["check"].as_str().unwrap_or_default().to_owned();
            row["waive"] = json!(waive_command(id, &check));
            row
        })
        .collect();
    Ok(json!({
        "change": id,
        "intent": found.record.intent.reference,
        "actor": found.record.actor.name,
        "op_id": found.op_id,
        "reason": found.reason,
        "waivable": rows,
        "commit": found.commit,
        "tree": found.tree,
        "diff": diff_summary(git, &found.base, &found.commit)?,
        "note": "a waiver is never a pass: the check lands as a named coverage gap, on this candidate only",
    }))
}

/// Whether `name` is enrolled in `registry`'s approver file.
fn enrolled(registry: &Registry, name: &str) -> bool {
    registry
        .principals(crate::registry::KeyClass::Approver)
        .iter()
        .any(|enrolled| enrolled == name)
}

/// N-9: `bay approve <change> --waive <check> --why <text>`: this machine's approver's signed
/// `waiver` record of `check`, a check the policy at the candidate's base marks `waivable =
/// true` and that did not pass, about exactly the pending candidate commit, with the reason.
/// It writes through `bay evidence record` ([`crate::outside`]). It refuses, writing nothing,
/// without a reason, without a registry (no waiver could count), for a name the registry does
/// not enrol as an approver, on a change held on no check, for a check that is not waivable,
/// and for one that has nothing to waive.
#[allow(clippy::too_many_lines)]
fn waive_in(
    dir: &Path,
    repo: &Repo,
    request: &Approve,
    name: &str,
    check: &str,
) -> Result<BayResult, String> {
    let id = request.change.as_str();
    if request.reject {
        return Ok(refused(
            id,
            "--waive and --reject ask two things: a waiver lets one check through, a rejection blocks the change",
            &waive_command(id, check),
            None,
        ));
    }
    let Some(why) = request.why.as_deref().filter(|why| !why.trim().is_empty()) else {
        return Ok(refused(
            id,
            &format!("a waiver names its reason, and no --why was given for check {check}"),
            &waive_command(id, check),
            None,
        ));
    };
    synchronize(repo)?;
    let Some(registry) = Registry::current(&repo.git)? else {
        return Ok(refused(
            id,
            &format!(
                "this repository has no key registry ({}), so no approver key can be verified and no waiver can count",
                crate::registry::REGISTRY_REF
            ),
            "enrol each approver in the registry's approver file, then waive again",
            None,
        ));
    };
    // N-9: only an enrolled approver's waiver counts; never one another name must give.
    if !enrolled(&registry, name) {
        return Ok(refused(
            id,
            &format!("{name} is not an enrolled approver, and only an approver's waiver counts"),
            "an enrolled approver waives it, or the registry's approver file enrols this name",
            Some(json!({"approver": name})),
        ));
    }
    let Some(record) = records(repo, Some(id))?.into_iter().next() else {
        return Ok(refused(
            id,
            &format!("change {id} is not on {} or on this machine", repo.remote()),
            READ_WAITING,
            None,
        ));
    };
    let Some(found) = waivable(repo, &record, Some(&registry))? else {
        let state = record.operations.last().map_or("never submitted", |op| {
            op.outcome.as_deref().unwrap_or(op_state(op))
        });
        return Ok(refused(
            id,
            &format!(
                "change {id} is held on no check a waiver can answer: its last operation is {state}"
            ),
            READ_WAITING,
            None,
        ));
    };
    // N-9: only a check the policy at the candidate's base marks waivable takes a waiver.
    let marked = match crate::policy::load_at(&repo.git, &found.base) {
        Ok(Some(policy)) => policy
            .checks
            .iter()
            .any(|entry| entry.id == check && entry.waivable && !entry.is_approval()),
        Ok(None) | Err(crate::policy::LoadError::Invalid(_)) => false,
        Err(crate::policy::LoadError::Git(why)) => return Err(why),
    };
    if !marked {
        return Ok(refused(
            id,
            &format!(
                "check {check} is not waivable: the policy at the base of candidate {} does not mark it waivable = true",
                found.commit
            ),
            "repair the change and land it again; a waiver answers only a [[check]] with waivable = true at the base",
            None,
        ));
    }
    let Some(row) = found.rows.iter().find(|row| row["check"] == check) else {
        return Ok(refused(
            id,
            &format!(
                "check {check} has nothing to waive on candidate {}: it passes, a waiver answers it already, or the candidate's diff does not require it",
                found.commit
            ),
            READ_WAITING,
            Some(json!({"waivable": found.rows})),
        ));
    };
    let written = outside::record(
        dir,
        &outside::Record {
            kind: evidence::KIND_WAIVER.to_owned(),
            // A waiver says nothing of the product: it is never a pass.
            verdict: "unknown".to_owned(),
            commits: vec![found.commit.clone()],
            check: Some(check.to_owned()),
            environment: row["environment"].as_str().map(str::to_owned),
            inputs: Vec::new(),
            body: None,
            version: None,
            why: Some(why.to_owned()),
        },
    );
    if written.decision != Decision::Ok {
        return Ok(written.with_change(id.to_owned()));
    }
    let records = written
        .detail
        .as_ref()
        .and_then(|detail| detail["records"].as_array().cloned())
        .unwrap_or_default();
    // A waiver never carries: a candidate that moved while it was written is not waived.
    if let Some(now) = records_now(repo, id).filter(|now| moved(now, &found.op_id, &found.commit)) {
        let newer = now
            .operations
            .last()
            .map(|op| op.op_id.clone())
            .unwrap_or_default();
        return Ok(refused(
            id,
            &format!(
                "the candidate moved: {name}'s waiver of {check} is recorded on candidate {}, and change {id} has a newer operation {newer} since",
                found.commit
            ),
            "read it again with bay waiting: a waiver names one candidate and never carries, so the new one is waived anew if it needs it",
            Some(json!({"records": records, "commit": found.commit})),
        ));
    }
    let view = repo.view_path(id);
    let actor = &found.record.actor.name;
    let (lands_by, next) = if view.is_dir() {
        (
            "here",
            format!(
                "this machine holds the change's view ({}): bay merge --wait there lands it, naming the waiver as a coverage gap",
                view.display()
            ),
        )
    } else {
        (
            "author",
            format!(
                "{actor}, the change's author, runs bay merge --wait, and the change lands with the waiver named as a coverage gap"
            ),
        )
    };
    Ok(BayResult::ok(json!({
        "approver": name,
        "waived": check,
        "environment": row["environment"],
        "verdict": row["verdict"],
        "commit": found.commit,
        "tree": found.tree,
        "records": records,
        "why": why,
        "lands_by": lands_by,
        "next": next,
    }))
    .with_change(id.to_owned())
    .with_op(found.op_id.clone()))
}

/// The change record as the remote holds it now; `None` when it cannot be read.
fn records_now(repo: &Repo, id: &str) -> Option<ChangeRecord> {
    records(repo, Some(id)).ok()?.into_iter().next()
}

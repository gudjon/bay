//! `bay start`: open a change, or resume one (`DESIGN` § 4).
//!
//! Built here (increment 1, basic): a fresh fetch of trunk, a sparse view of the declared
//! paths, the change record and its chain, pushed to the remote; `--resume` checkpoints the
//! draft, brings in a newer trunk and reports what moved. Intent claims and adoption are
//! kickstart item 19e; the view journal is item 7; the shared machine cache is item 15.

use std::path::Path;

use serde_json::json;

use crate::change::{
    Actor, Chain, ChangeRecord, ChangeState, Intent, IntentClaim, RECORD_SCHEMA, StartedFrom,
    actor_source, change_ref, commit_on, current_actor, freeze_draft, intent_hash,
};
use crate::git::Git;
use crate::ids::{is_change_id, new_change_id};
use crate::inputs::{self, DeclaredInput, InputName};
use crate::merge::{Merge, merge_onto};
use crate::registry::Registry;
use crate::repo::Repo;
use crate::result::{BayResult, Decision, Reason};
use crate::signing::{Keys, Signer};
use crate::view;

#[derive(Clone)]
pub struct StartRequest {
    pub intent: Option<String>,
    /// Claim the intent's next generation after its change ended without landing, instead of
    /// adopting it (`start --new`, G272). After a landing every start claims the next generation
    /// (G293), so there `--new` changes nothing.
    pub new: bool,
    pub intent_revision: String,
    pub paths: Vec<String>,
    /// The named inputs given to `--input` (`DESIGN` § 3.1, ruling R3).
    pub inputs: Vec<InputName>,
    pub resume: Option<String>,
    /// The landed change this one undoes (`bay undo`); it becomes the `Reverts:` trailer.
    pub reverts: Option<String>,
    /// The landed change this one fixes (`start --fixes`, G161); it becomes the `Fixes:` trailer.
    /// Unlike `--new`, it claims no generation of that change's intent.
    pub fixes: Option<String>,
    /// The sandbox lane of `.bay/config.toml` this change works in (`start --lane`, NEW-4): its
    /// checks run against the current trunk and it never lands.
    pub lane: Option<String>,
    /// A branch made outside bay this change takes in (`start --from`, G268): a branch name or a
    /// full ref. Its net change since its merge base with trunk is the change's first draft.
    pub from: Option<String>,
    /// The changes this one lands only after (`start --after`, repeatable, G273). Each must be a
    /// change the remote holds, other than this one; none need have landed yet.
    pub after: Vec<String>,
    /// The main checkout's edits this change takes in (`bay save`, G279): merged onto the base
    /// as the change's first draft.
    pub saved: Option<Saved>,
}

/// The edits made in the main checkout that `bay save` takes in (G279).
#[derive(Clone)]
pub struct Saved {
    /// A commit of the edits on the main checkout's `HEAD`, which is on trunk's history, so its
    /// merge onto the base has that `HEAD` as its merge base.
    pub commit: String,
    /// The first draft's commit message (`bay save --message`).
    pub message: String,
}

/// How long a start whose claim push met a held remote ref lock waits before it re-reads.
const CLAIM_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

/// The paths `--paths` names, as the change declares them: `.` or `/` (the most natural first
/// guess for "all of it") names the whole tree, which a change declares by naming no path, so
/// its view is a full checkout and takes a new file anywhere. As a sparse rule, `/.` or `/`
/// matched no file, and every new file was refused.
#[must_use]
pub fn declared_paths(paths: Vec<String>) -> Vec<String> {
    let whole = |path: &String| matches!(path.trim().trim_end_matches('/'), "" | ".");
    if paths.iter().any(whole) {
        Vec::new()
    } else {
        paths
    }
}

#[must_use]
pub fn start(dir: &Path, request: &StartRequest) -> BayResult {
    let repo = match Repo::open_fetched(dir) {
        Ok(repo) => repo,
        Err(error) => return error.into_result(),
    };
    // G265 (N-13): a box below its disk floor (`bay.minFreeMiB`) starts nothing: no change and
    // no view, measured where the views are made.
    match crate::disk::below_floor(&repo.git, &repo.views_dir()) {
        Ok(None) => {}
        Ok(Some(low)) => {
            let mut refused = BayResult::blocked(Reason::DiskLow).with_detail(low.detail());
            refused.freshness = repo.freshness();
            return refused;
        }
        Err(why) => return BayResult::failed(why),
    }
    let resumed = request
        .resume
        .clone()
        .or_else(|| names_change(&repo, request));
    let result = match &resumed {
        Some(id) => resume(&repo, id, request.from.as_deref()),
        None => open(&repo, request),
    };
    let mut result = result.unwrap_or_else(BayResult::failed);
    // NEW-8: a change opened or adopted hears of the live changes that declare its files.
    if resumed.is_none() {
        list_overlaps(&repo, &mut result);
    }
    // G175: the opt-in warm-up of the view's history, once the view is made.
    if repo.config.warm_history() {
        warm_history(&repo, &mut result);
    }
    // G97: views of ended changes past the retention window go, newer work never.
    let released = release_ended(&repo, dir);
    if !released.is_empty()
        && let Some(detail) = result
            .detail
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
    {
        detail.insert("released_views".to_owned(), json!(released));
    }
    // G80: every result says which trunk it reflects, and when that trunk was fetched.
    result.freshness = result.freshness.or_else(|| repo.freshness());
    result
}

/// G300: the change a plain `bay start <CHANGE>` picks up, as `--resume <CHANGE>` does. The
/// positional names a change when it is a change id and that change's ref is here or on the
/// remote; otherwise it is an intent, as before. A start that asks for what only a new change
/// takes (`--new`, `--fixes`, `--lane`, `--after`, a save) always opens one. `--from` with a
/// change named replaces its draft (`resume`).
fn names_change(repo: &Repo, request: &StartRequest) -> Option<String> {
    let id = request.intent.as_deref().filter(|id| is_change_id(id))?;
    if request.new
        || request.reverts.is_some()
        || request.fixes.is_some()
        || request.lane.is_some()
        || !request.after.is_empty()
        || request.saved.is_some()
    {
        return None;
    }
    let here = Chain::new(&repo.git).head(id).ok().flatten().is_some();
    (here || repo.fetch_refs(&[change_ref(id)]).is_ok()).then(|| id.to_owned())
}

/// NEW-8: a start that opened or adopted a change lists, in its detail, the live changes whose
/// declared paths overlap the change's own (`overlaps`, `crate::overlap`), from the chains this
/// repository already holds, or why they could not be read (`overlaps_unread`). It never
/// refuses: the listing is what the work may meet.
fn list_overlaps(repo: &Repo, result: &mut BayResult) {
    if result.decision != Decision::Ok {
        return;
    }
    let Some(id) = result.change.clone() else {
        return;
    };
    let Some(detail) = result
        .detail
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    let listed = Chain::new(&repo.git)
        .read(&id)
        .map_err(|error| error.message())
        .and_then(|(_, record)| {
            crate::overlap::overlaps(repo, &id, &inputs::paths(&record.declared_inputs))
        });
    let (key, value) = match listed {
        Ok(overlaps) => ("overlaps", json!(overlaps)),
        Err(why) => ("overlaps_unread", json!(why)),
    };
    detail.insert(key.to_owned(), value);
}

/// G175: with `[view] warm_history`, a start that made or kept a view fetches the history of
/// its change's declared paths there, and its detail says how much (`history`).
fn warm_history(repo: &Repo, result: &mut BayResult) {
    if result.decision != Decision::Ok {
        return;
    }
    let Some(id) = result.change.clone() else {
        return;
    };
    let Some(detail) = result
        .detail
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    let Some(view_path) = detail.get("view").and_then(serde_json::Value::as_str) else {
        return;
    };
    let view_path = std::path::PathBuf::from(view_path);
    let Ok((_, record)) = Chain::new(&repo.git).read(&id) else {
        return;
    };
    let declared = inputs::paths(&record.declared_inputs);
    detail.insert(
        "history".to_owned(),
        view::warm_history(&view_path, &declared),
    );
}

/// G97: before a new change, release this machine's views of ended changes past the retention
/// window, keeping any that holds newer work than the revision its change ended with. The view
/// `dir` stands in, and the view of a live session on this machine (its latest work record line
/// is fresh and not ended), are in use and kept.
fn release_ended(repo: &Repo, dir: &Path) -> Vec<String> {
    let chain = Chain::new(&repo.git);
    let mut in_use: Vec<String> = repo.view_change(dir).into_iter().collect();
    let lines = crate::live::lines(repo).unwrap_or_default();
    let rows = crate::live::rows_of(
        lines,
        crate::live::unix_now(),
        repo.config.takeover().as_secs(),
    );
    in_use.extend(
        rows.iter()
            .filter_map(|row| row["change"].as_str().map(str::to_owned)),
    );
    let ended = |id: &str| {
        if in_use.iter().any(|used| used == id) {
            return None;
        }
        let (head, record) = chain.read(id).ok()?;
        // NEW-4: a lane change never lands. Once its checks ran (`lane::CHECKED`) its work is done,
        // and its view goes after the lane's shorter window; one of a lane the config no longer
        // declares keeps the retention window.
        let lane_window = record.lane.as_deref().map(|lane| {
            repo.config.lane(lane).map_or_else(
                || repo.config.retention_window(),
                |lane| repo.config.lane_window(lane),
            )
        });
        let never_lands = lane_window.is_some()
            && record
                .operations
                .last()
                .is_some_and(|op| op.outcome.as_deref() == Some(crate::lane::CHECKED));
        let ended = never_lands
            || matches!(
                record.state,
                ChangeState::Landed | ChangeState::Abandoned | ChangeState::NoEffect
            );
        if !ended {
            return None;
        }
        let mut placed = chain.drafts(id).ok()?;
        let revision = crate::status::last_revision(&record, placed.first()?);
        // The change ended with its chain's last state commit; a time that cannot be read is
        // never past the window.
        let at_unix: u64 = repo
            .git
            .run(&["log", "-1", "--format=%ct", &head])
            .ok()?
            .trim()
            .parse()
            .ok()?;
        placed.extend([record.base, record.base_first]);
        Some(view::Ended {
            revision,
            at_unix,
            placed,
            window: lane_window,
        })
    };
    // Owner ruling 2026-10-26: a view of an ended change that holds nothing newer is released at
    // once. Its work is on main or recorded on the chain; the retention window keeps the chain's
    // records, never a view (a fleet's shared checkout held 574 views). A lane change keeps its
    // own window.
    view::release_ended(repo, std::time::Duration::ZERO, &ended)
}

/// The refusal of a start whose `--lane` the config at trunk does not declare, whose `--fixes`
/// names no change on trunk at `base`, or whose `--after` names no change; nothing is created
/// then.
fn named_or_refused(
    repo: &Repo,
    request: &StartRequest,
    base: &str,
) -> Result<Option<BayResult>, String> {
    // Fleet findings (2026-10-27): `bay start <change> --after <id>` (or `--fixes`) opened a new,
    // empty change whose intent was that change's id, while its author meant to refresh it. A
    // change named with what only a new change takes is refused, never a second change by it.
    if let Some(id) = request.intent.as_deref().filter(|id| is_change_id(id))
        && !request.new
        && (request.fixes.is_some()
            || request.lane.is_some()
            || request.reverts.is_some()
            || !request.after.is_empty())
        && (Chain::new(&repo.git).head(id).ok().flatten().is_some()
            || repo.fetch_refs(&[change_ref(id)]).is_ok())
    {
        return Err(format!(
            "{id} is a change: `bay start {id}` alone refreshes it in place and keeps its place; \
             --after, --fixes, --lane and --reverts open a new change, so give that one its own \
             intent, or pass --new"
        ));
    }
    // Fleet findings (2026-10-27): `--paths a,b,c` was read as one path named `a,b,c`.
    if let Some(joined) = request.paths.iter().find(|path| {
        path.contains(',')
            && repo
                .git
                .run(&[
                    "ls-tree",
                    "--name-only",
                    base,
                    "--",
                    path.trim_end_matches('/'),
                ])
                .is_ok_and(|listed| listed.is_empty())
    }) {
        return Err(format!(
            "--paths {joined}: paths are separated by spaces, not commas; main holds no path by \
             that name"
        ));
    }
    // NEW-4: a lane is one the config at trunk declares, or nothing is created.
    if let Some(lane) = &request.lane
        && repo.config.lane(lane).is_none()
    {
        let declared: Vec<&str> = repo
            .config
            .lanes
            .iter()
            .map(|lane| lane.name.as_str())
            .collect();
        return Err(format!(
            "--lane {lane}: the {} on main declares no such [[lane]] (it declares {declared:?})",
            crate::config::CONFIG_PATH
        ));
    }
    // G161: a fix names a change on trunk, or nothing is created.
    if let Some(fixed) = &request.fixes {
        if !is_change_id(fixed) {
            return Err(format!("--fixes {fixed} is not a change id"));
        }
        if crate::undo::landed_commit(repo, fixed, base)?.is_none() {
            return Ok(Some(
                BayResult::blocked(Reason::ChangeNotLanded)
                    .with_change(fixed.clone())
                    .with_detail(json!({"trunk": base, "fixes": fixed,
                    "why": "a fix names a change that merged"})),
            ));
        }
    }
    // G273: a dependency is a change the remote holds; it need not have landed.
    for dependency in &request.after {
        if !is_change_id(dependency) {
            return Err(format!("--after {dependency} is not a change id"));
        }
        let landed = format!("{}{dependency}", crate::names::LANDED);
        if !remote_has(repo, &change_ref(dependency))? && !remote_has(repo, &landed)? {
            return Err(format!(
                "--after {dependency}: the remote holds no such change"
            ));
        }
    }
    Ok(None)
}

/// G273: `request`'s `--after` names `change` itself, or a change that lands only after
/// `change` (directly or through others), so neither could ever land: the refusal, with nothing
/// written. The walk reads each named change's record from the remote, at most
/// [`AFTER_WALK_MAX`] of them.
fn after_itself(repo: &Repo, request: &StartRequest, change: &str) -> Option<String> {
    if request.after.iter().any(|dependency| dependency == change) {
        return Some(format!(
            "--after {change} names the change itself: a change cannot merge after itself"
        ));
    }
    let mut seen: Vec<String> = Vec::new();
    let mut next: Vec<String> = request.after.clone();
    while let Some(dependency) = next.pop() {
        if seen.contains(&dependency) || seen.len() >= AFTER_WALK_MAX {
            continue;
        }
        seen.push(dependency.clone());
        // Read at the remote's head, never moving a local copy of the chain back (G199).
        let Ok(crate::repo::RemoteRecord { record, .. }) = repo.remote_record(&dependency) else {
            continue;
        };
        if record.after.iter().any(|named| named == change) {
            return Some(format!(
                "--after {dependency}: change {dependency} merges only after {change}, so \
                 neither could merge"
            ));
        }
        next.extend(record.after);
    }
    None
}

/// The most dependency records [`after_itself`] reads.
const AFTER_WALK_MAX: usize = 64;

fn open(repo: &Repo, request: &StartRequest) -> Result<BayResult, String> {
    let intent = request
        .intent
        .clone()
        .ok_or("bay start needs an intent, or a change id")?;
    let base = repo.fetch_trunk().map_err(|error| error.message())?;
    let me = current_actor(&repo.common_dir)?;
    let source = actor_source();
    // G152: a policy at the base that requires a named actor refuses a name from a default,
    // before any claim is written.
    if source.is_default() && crate::policy::requires_actor_at(&repo.git, &base)? {
        return Ok(BayResult::blocked(Reason::ActorUnnamed).with_detail(json!({
            "actor": me.name,
            "actor_source": source,
            "policy": crate::policy::POLICY_PATH,
            "base": base,
            "recovery": "set BAY_ACTOR to this session's own actor name and start again",
        })));
    }
    if let Some(refused) = named_or_refused(repo, request, &base)? {
        return Ok(refused);
    }
    // G264: the most live changes one actor may hold, as the policy at the base sets it.
    let limit = crate::policy::max_live_changes_at(&repo.git, &base)?;
    // G268: the outside branch is resolved, judged against the declared paths and merged onto
    // the base before any claim is written, so a refusal creates nothing. G279: so are the
    // main checkout's edits a save takes in.
    let taken_in = match (&request.from, &request.saved) {
        (None, None) => None,
        (Some(reference), _) => match take_in(repo, reference, &base, &request.paths)? {
            Ok(taken_in) => Some(taken_in),
            Err(refusal) => return Ok(refusal),
        },
        (None, Some(saved)) => match take_saved(repo, saved, &base)? {
            Ok(taken_in) => Some(taken_in),
            Err(refusal) => return Ok(refusal),
        },
    };
    // G268: an outside branch with no `--paths` declares the paths it changes, never the whole
    // tree, so its claim holds only what it touches.
    let derived;
    let request = match &taken_in {
        Some(taken) if request.paths.is_empty() && request.from.is_some() => {
            derived = StartRequest {
                paths: taken.changed.clone(),
                ..request.clone()
            };
            &derived
        }
        _ => request,
    };
    let hash = intent_hash(&intent, &request.intent_revision);
    // § 4: one intent, one change, by claim. A lost race re-reads the claims.
    for _ in 0..5 {
        let claims = remote_claims(repo, &hash)?;
        let (generation, supersedes) = match claims.last_key_value() {
            None => (1, None),
            Some((generation, change)) => {
                let landed = remote_has(repo, &format!("{}{change}", crate::names::LANDED))?;
                match (landed, request.new) {
                    // G293: a landed change closes its generation, so any start claims the
                    // next one, `--new` or not; the new change supersedes the landed one.
                    (true, _) => (generation + 1, Some(change.clone())),
                    // G272: a change that ended without landing closes its generation too.
                    (false, true) if ended_unlanded(fetched_record(repo, change)?.state) => {
                        (generation + 1, Some(change.clone()))
                    }
                    (false, true) => {
                        return Ok(BayResult::blocked(Reason::ChangeSuperseded)
                            .with_change(change.clone())
                            .with_detail(json!({"intent": intent, "generation": generation,
                                "why": format!("change {change} of generation {generation} is \
                                    live: the next generation needs it merged or given up"),
                                "recovery": format!("bay abandon from its view gives it \
                                    up; bay start {intent} without --new adopts it")})));
                    }
                    (false, false) => {
                        return adopt_or_refuse(repo, request, &me, change, &base, limit, 1);
                    }
                }
            }
        };
        // G264: a new change is one more the actor holds; at the limit nothing is created.
        if let Some(refusal) = wip_limit(repo, &base, limit, &me)? {
            return Ok(refusal);
        }
        let claim = IntentClaim {
            hash: hash.clone(),
            generation,
        };
        let opened = create(
            repo,
            request,
            &intent,
            &me,
            &base,
            claim,
            supersedes,
            taken_in.as_ref(),
        )?;
        if let Some(result) = opened {
            return Ok(result);
        }
    }
    Err(format!("the claim on {intent} kept moving; try again"))
}

/// `wip_limit` for adopting a change `owner` holds: adopting one's own change adds none.
fn adopt_limit(
    repo: &Repo,
    base: &str,
    limit: Option<u64>,
    me: &Actor,
    owner: &str,
) -> Result<Option<BayResult>, String> {
    if owner == me.name {
        return Ok(None);
    }
    wip_limit(repo, base, limit, me)
}

/// G264 (N-11): under a policy at the base with `[agent] max_live_changes = N`, the refusal
/// `WIP_LIMIT` when `me` already holds N live changes (not landed, abandoned or superseded; a
/// lane change counts), by the chains this repository holds. Its detail names each one, its
/// change id and intent. No limit refuses nothing.
fn wip_limit(
    repo: &Repo,
    base: &str,
    limit: Option<u64>,
    me: &Actor,
) -> Result<Option<BayResult>, String> {
    let Some(limit) = limit else {
        return Ok(None);
    };
    let live = crate::overlap::held_by(repo, &me.name)?;
    if u64::try_from(live.len()).unwrap_or(u64::MAX) < limit {
        return Ok(None);
    }
    Ok(Some(BayResult::blocked(Reason::WipLimit).with_detail(json!({
        "actor": me.name,
        "max_live_changes": limit,
        "live_changes": live,
        "policy": crate::policy::POLICY_PATH,
        "base": base,
        "recovery": "merge one of the live changes, or give one up with bay abandon, then start again",
    }))))
}

/// Whether the remote holds a claim on the intent, so a further start claims its next
/// generation (`bay undo`, § 4).
///
/// # Errors
///
/// A git fault.
pub fn claimed(repo: &Repo, intent: &str, revision: &str) -> Result<bool, String> {
    Ok(!remote_claims(repo, &intent_hash(intent, revision))?.is_empty())
}

/// The claims on an intent the remote holds: generation → change id.
fn remote_claims(
    repo: &Repo,
    hash: &str,
) -> Result<std::collections::BTreeMap<u64, String>, String> {
    let prefix = format!("{}{hash}/", crate::names::INTENT);
    let _ = repo.git.run(&[
        "fetch",
        "-q",
        "--no-tags",
        repo.remote(),
        &format!("+{prefix}*:{prefix}*"),
    ]);
    let refs = repo
        .git
        .run(&["for-each-ref", "--format=%(objectname) %(refname)", &prefix])
        .map_err(|error| error.message())?;
    let chain = Chain::new(&repo.git);
    let mut claims = std::collections::BTreeMap::new();
    for line in refs.lines() {
        let Some((commit, name)) = line.split_once(' ') else {
            continue;
        };
        let Some(generation) = name
            .strip_prefix(&prefix)
            .and_then(|number| number.parse().ok())
        else {
            continue;
        };
        let record = chain.record_at(commit).map_err(|error| error.message())?;
        claims.insert(generation, record.id);
    }
    compacted_claims(repo, hash, &mut claims)?;
    Ok(claims)
}

/// Whether `state` ends a change without a landing (G272): abandoned, superseded or no effect.
/// Such a change closes its generation, so `start --new` may claim the next one; a plain start
/// adopts it.
fn ended_unlanded(state: ChangeState) -> bool {
    matches!(
        state,
        ChangeState::Abandoned | ChangeState::Superseded | ChangeState::NoEffect
    )
}

/// The current record of `change`'s chain as the remote holds it ([`Repo::remote_record`]):
/// this store's chain moves only forward, and keeps local work the remote lacks (G289).
fn fetched_record(repo: &Repo, change: &str) -> Result<ChangeRecord, String> {
    Ok(repo.remote_record(change)?.record)
}

/// G272: the newer generation of `record`'s intent that the remote claims, when `record` ended
/// without landing: generation and change id. Re-opening such a change would put two live
/// changes on one intent, so adoption and resume refuse it.
fn newer_generation(repo: &Repo, record: &ChangeRecord) -> Result<Option<(u64, String)>, String> {
    let Some(claim) = &record.claim else {
        return Ok(None);
    };
    if !ended_unlanded(record.state) {
        return Ok(None);
    }
    let claims = remote_claims(repo, &claim.hash)?;
    Ok(claims
        .range(claim.generation + 1..)
        .next_back()
        .map(|(generation, change)| (*generation, change.clone())))
}

/// `CHANGE_SUPERSEDED` for change `id`, which ended without landing behind the newer
/// generation `newer` of its intent (G272).
fn superseded_by_newer(id: &str, record: &ChangeRecord, newer: &(u64, String)) -> BayResult {
    let (generation, change) = newer;
    BayResult::blocked(Reason::ChangeSuperseded)
        .with_change(id)
        .with_detail(json!({
            "intent": record.intent.reference,
            "state": record.state,
            "superseded_by": change,
            "generation": generation,
            "why": format!("change {id} ended without merging and generation {generation} of \
                its intent is change {change}"),
            "recovery": format!("bay start {change} continues the intent"),
        }))
}

/// Whether the remote holds the marker `name`: as a ref, or compacted into its shard index
/// (§ 10a), where the record stands in for the ref.
fn remote_has(repo: &Repo, name: &str) -> Result<bool, String> {
    Ok(crate::compact::remote_marker(repo, name)?.is_some())
}

/// The claims on `hash` the shard indexes record, beyond `claims` (§ 10a): generations run
/// from 1 without a gap, so each missing one is looked up until one is recorded nowhere. A
/// claim was compacted only with its landed claim, whose publication names the change.
fn compacted_claims(
    repo: &Repo,
    hash: &str,
    claims: &mut std::collections::BTreeMap<u64, String>,
) -> Result<(), String> {
    crate::compact::fetch(repo)?;
    for generation in 1.. {
        if claims.contains_key(&generation) {
            continue;
        }
        let claim = IntentClaim {
            hash: hash.to_owned(),
            generation,
        };
        if crate::compact::recorded(&repo.git, &claim.intent_ref())?.is_none() {
            return Ok(());
        }
        // The shards are fetched: a compacted landed claim needs no further remote read.
        let landed = match crate::compact::recorded(&repo.git, &claim.landed_ref())? {
            Some(landed) => Some(landed),
            None => crate::compact::remote_marker(repo, &claim.landed_ref())?,
        }
        .ok_or_else(|| {
            format!(
                "{} is compacted without its merged claim",
                claim.intent_ref()
            )
        })?;
        let change = repo
            .git
            .run(&[
                "log",
                "-1",
                "--format=%(trailers:key=Change-Id,valueonly,separator=%x2c)",
                &landed,
            ])
            .map_err(|error| error.message())?;
        let change = change.trim();
        if !is_change_id(change) {
            return Err(format!(
                "the merge {landed} of {} names no one change",
                claim.landed_ref()
            ));
        }
        claims.insert(generation, change.to_owned());
    }
    Ok(())
}

/// An outside branch a start takes in (`start --from`, G268): what the record keeps of it, and
/// the draft on the base that holds its net change. The main checkout's edits a save takes in
/// (G279) keep no `from`.
struct TakenIn {
    from: Option<StartedFrom>,
    draft: String,
    /// The paths the outside branch changes since its merge base with trunk: what a start
    /// with `--from` and no `--paths` declares (G268).
    changed: Vec<String>,
}

/// Takes in the branch made outside bay that `reference` names (G268): the commit the remote's
/// branch or ref of that name holds, or this repository's only when the remote has none
/// ([`resolve_from`]); every path it changes since its merge base with trunk, which the
/// declared `paths` must cover, or the start is the `PATHS_UNDECLARED` refusal; and the
/// three-way merge of it onto `base` (`merge_onto`, the merge a candidate is built with),
/// committed on `base` as the change's first draft. A conflict is the `MERGE_CONFLICT` refusal;
/// nothing is written but objects no ref names.
fn take_in(
    repo: &Repo,
    reference: &str,
    base: &str,
    paths: &[String],
) -> Result<Result<TakenIn, BayResult>, String> {
    let (commit, source) = match resolve_from(repo, reference)? {
        Resolved::Commit(commit, source) => (commit, source),
        Resolved::Diverged {
            full,
            remote,
            local,
        } => {
            return Ok(Err(BayResult::blocked(Reason::BranchDiverged).with_detail(json!({
                "ref": reference,
                "full": full,
                "remote": remote,
                "local": local,
                "why": format!("--from {reference}: the remote's {full} is {remote}, and this repository's is {local}, with commits the remote's lacks"),
            }))));
        }
    };
    let from = StartedFrom {
        reference: reference.to_owned(),
        commit: commit.clone(),
        source: Some(source.to_owned()),
    };
    let merge_base = repo.git.run(&["merge-base", base, &commit]).map_err(|_| {
        format!("--from {reference}: {commit} shares no history with main ({base})")
    })?;
    // `--no-renames`: which paths changed is read from trees alone, never from blob contents.
    let changed = repo
        .git
        .run(&[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            &merge_base,
            &commit,
        ])
        .map_err(|error| error.message())?;
    let changed: Vec<String> = changed
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect();
    // A change that declares no paths holds the whole tree.
    let uncovered: Vec<&str> = changed
        .iter()
        .map(String::as_str)
        .filter(|path| {
            !paths.is_empty() && !paths.iter().any(|declared| inputs::within(declared, path))
        })
        .collect();
    // N-5: a branch that changes a path the change does not declare is refused, never taken
    // in whole: nothing is created, and a start whose `--paths` cover them can succeed.
    if !uncovered.is_empty() {
        return Ok(Err(BayResult::blocked(Reason::PathsUndeclared)
            .with_detail(json!({
                "uncovered": uncovered,
                "declared": paths,
                "from": from,
                "why": format!("--from {reference} changes paths --paths does not declare"),
                "recovery": "start again with --paths covering them",
            }))));
    }
    match merge_onto(&repo.git, base, &commit).map_err(|error| error.message())? {
        Merge::Conflict(conflicts, _) => Ok(Err(BayResult::blocked(Reason::MergeConflict)
            .with_detail(json!({
                "conflicts": conflicts,
                "from": from,
                "trunk": base,
                "next": "bring main into the branch outside bay, then start again",
            })))),
        Merge::Clean(tree) => {
            // The branch's own words, oldest first as a squash says them, and its author.
            let messages = repo
                .git
                .run(&[
                    "log",
                    "--reverse",
                    "--format=%B%x00",
                    &format!("{merge_base}..{commit}"),
                ])
                .map_err(|error| error.message())?;
            let message: Vec<&str> = messages
                .split('\0')
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .collect();
            let message = if message.is_empty() {
                format!("start --from {reference}")
            } else {
                message.join("\n\n")
            };
            let draft = crate::change::commit_on_by(&repo.git, &tree, base, &message, &commit)
                .map_err(|error| error.message())?;
            Ok(Ok(TakenIn {
                from: Some(from),
                draft,
                changed,
            }))
        }
    }
}

/// Takes in the main checkout's edits (`bay save`, G279): `saved.commit` holds them on the
/// checkout's `HEAD`, which is on trunk's history, so the three-way merge onto `base`
/// (`merge_onto`, the merge a candidate is built with) has that `HEAD` as its merge base. The
/// merge, committed on `base` with the save's message, is the change's first draft. A conflict
/// with newer trunk is the `MERGE_CONFLICT` refusal; nothing is written but objects no ref
/// names, and the main checkout is never touched.
fn take_saved(
    repo: &Repo,
    saved: &Saved,
    base: &str,
) -> Result<Result<TakenIn, BayResult>, String> {
    match merge_onto(&repo.git, base, &saved.commit).map_err(|error| error.message())? {
        Merge::Conflict(conflicts, _) => Ok(Err(BayResult::blocked(Reason::MergeConflict)
            .with_detail(json!({
                "conflicts": conflicts,
                "trunk": base,
                "next": "main changed the same lines since the main checkout's HEAD: bring \
                    those files up to main, redo the edits there, then save again",
            })))),
        Merge::Clean(tree) => {
            let draft = commit_on(&repo.git, &tree, base, &saved.message)
                .map_err(|error| error.message())?;
            Ok(Ok(TakenIn {
                from: None,
                draft,
                changed: Vec::new(),
            }))
        }
    }
}

/// The commit `reference` names (G268), and where it was read (N-5): `remote` when the remote
/// has that branch (`<name>` is `refs/heads/<name>`) or full ref, fetched into a ref of bay's
/// own that is deleted once read, even when this repository holds a branch of the same name,
/// which may be stale; `local` only when the remote has none, as a full ref, a branch or any
/// revision this repository holds. A remote bay cannot list is an error, never a fall back to
/// a local copy.
fn resolve_from(repo: &Repo, reference: &str) -> Result<Resolved, String> {
    if reference.is_empty() || reference.starts_with('-') {
        return Err(format!("--from {reference:?} names no branch or ref"));
    }
    let remote = repo.remote();
    let full = if reference.starts_with("refs/") {
        reference.to_owned()
    } else {
        format!("refs/heads/{reference}")
    };
    let listed = repo
        .git
        .run(&["ls-remote", remote, &full])
        .map_err(|error| {
            format!(
                "--from {reference}: the remote {remote} cannot be listed: {}",
                error.message()
            )
        })?;
    // `ls-remote` matches a pattern's tail: only the exact name is the remote's.
    let on_remote = listed.lines().any(|line| {
        line.split_once('\t')
            .is_some_and(|(_, name)| name.trim() == full)
    });
    if on_remote {
        let remote_tip = fetch_from(repo, remote, reference, &full)?;
        // N-5: a local branch of the name that is only behind the remote's is stale, and the
        // remote's is taken. One that holds commits the remote's lacks (a local rebase, an
        // unpushed commit) is never dropped silently: the start names both.
        if let Ok(local) =
            repo.git
                .run(&["rev-parse", "--verify", "-q", &format!("{full}^{{commit}}")])
            && local != remote_tip
            && repo
                .git
                .run(&["merge-base", "--is-ancestor", &local, &remote_tip])
                .is_err()
        {
            return Ok(Resolved::Diverged {
                full,
                remote: remote_tip,
                local,
            });
        }
        return Ok(Resolved::Commit(remote_tip, "remote"));
    }
    repo.git
        .run(&[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{reference}^{{commit}}"),
        ])
        .map(|commit| Resolved::Commit(commit, "local"))
        .map_err(|_| {
            format!(
                "--from {reference}: the remote {remote} has no {full}, and this repository holds \
                 no such branch, ref or commit"
            )
        })
}

/// What `--from <reference>` names ([`resolve_from`]).
enum Resolved {
    /// The commit, and where it was read: `remote` or `local`.
    Commit(String, &'static str),
    /// The remote has the branch, and this repository's copy holds commits the remote's lacks:
    /// the `BRANCH_DIVERGED` refusal, naming both.
    Diverged {
        full: String,
        remote: String,
        local: String,
    },
}

/// The commit the remote's `full` holds, fetched into a ref of bay's own that is deleted once
/// read.
fn fetch_from(repo: &Repo, remote: &str, reference: &str, full: &str) -> Result<String, String> {
    let fetched = format!("{}from-{}", crate::names::FETCH, std::process::id());
    let commit = repo
        .git
        .run(&[
            "fetch",
            "-q",
            "--no-tags",
            remote,
            &format!("+{full}:{fetched}"),
        ])
        .and_then(|_| {
            repo.git.run(&[
                "rev-parse",
                "--verify",
                "-q",
                &format!("{fetched}^{{commit}}"),
            ])
        });
    let _ = repo.git.run(&["update-ref", "-d", &fetched]);
    commit.map_err(|error| {
        format!(
            "--from {reference}: the remote {remote} has {full}, and it cannot be \
                     fetched: {}",
            error.message()
        )
    })
}

/// Creates the change and its claim in one atomic push (§ 4), so a crash never leaves a claim
/// without a change. Returns `None` when another start claimed the generation first. A change
/// that takes in an outside branch (G268) starts from its draft rather than from `base`.
#[allow(clippy::too_many_arguments)]
fn create(
    repo: &Repo,
    request: &StartRequest,
    intent: &str,
    me: &Actor,
    base: &str,
    claim: IntentClaim,
    supersedes: Option<String>,
    taken_in: Option<&TakenIn>,
) -> Result<Option<BayResult>, String> {
    let id = new_change_id().map_err(|error| error.to_string())?;
    let intent_ref = claim.intent_ref();
    let declared_inputs = request
        .paths
        .iter()
        .cloned()
        .map(DeclaredInput::Path)
        .chain(
            request
                .inputs
                .iter()
                .cloned()
                .map(|input| DeclaredInput::Named { input }),
        )
        .collect();
    let mut record = ChangeRecord {
        schema: RECORD_SCHEMA.to_owned(),
        id: id.clone(),
        intent: Intent {
            reference: intent.to_owned(),
            revision: request.intent_revision.clone(),
        },
        actor: me.clone(),
        actor_source: Some(actor_source()),
        base_first: base.to_owned(),
        base: base.to_owned(),
        observed_remote: base.to_owned(),
        declared_inputs,
        observed_inputs: Vec::new(),
        observed_versions: std::collections::BTreeMap::new(),
        validated_candidate: None,
        state: ChangeState::Open,
        superseded_by: None,
        claim: Some(claim),
        supersedes,
        reverts: request.reverts.clone(),
        fixes: request.fixes.clone(),
        adopted_from: Vec::new(),
        operations: Vec::new(),
        retention_cut: false,
        receipts: Vec::new(),
        lane: request.lane.clone(),
        from: taken_in.and_then(|taken_in| taken_in.from.clone()),
        after: dedup(&request.after),
    };
    let draft = taken_in.map_or(base, |taken_in| taken_in.draft.as_str());
    // § 8: the view is of `base`, so what the actor reads there is what it observed.
    record.observed_versions = inputs::observe(repo, &record, base);
    let mut chain = Chain::new(&repo.git);
    chain.sign_with(agent_signer(repo, &me.name)?);
    let head = chain
        .create(&record, draft, &format!("start {intent}"))
        .map_err(|error| error.message())?;
    let name = change_ref(&id);
    // G198: both refs are create-only. A lease that expects no ref refuses one that exists,
    // even when the new value fast-forwards it, so of racing starts exactly one creates the
    // claim and every other one re-reads the claims.
    if let Err(error) = repo.git.run(&[
        "push",
        "-q",
        "--atomic",
        &format!("--force-with-lease={name}:"),
        &format!("--force-with-lease={intent_ref}:"),
        repo.remote(),
        &format!("{name}:{name}"),
        &format!("{head}:{intent_ref}"),
    ]) {
        let _ = repo.git.run(&["update-ref", "-d", &name, &head]);
        if remote_has(repo, &intent_ref)? {
            return Ok(None);
        }
        // The remote's ref lock was held by a writer that has not committed yet: the claim is
        // undecided, never a fault. Re-read the claims after a short wait.
        if error.message().contains("cannot lock ref") {
            std::thread::sleep(CLAIM_LOCK_WAIT);
            return Ok(None);
        }
        return Err(format!(
            "the change and its claim could not be pushed: {}",
            error.message()
        ));
    }
    // The claim is the remote's now; its local copy is created, never moved (G198).
    let _ = repo.git.run(&["update-ref", &intent_ref, &head, ""]);
    let view = view::create(repo, &id, draft, &request.paths).map_err(|error| error.message())?;
    Ok(Some(
        BayResult::ok(json!({
            "view": view.display().to_string(),
            "base": base,
            "paths": request.paths,
            "inputs": request.inputs,
            "observed_versions": record.observed_versions,
            "intent": intent_ref,
            "actor": record.actor,
            "actor_source": record.actor_source,
            "fixes": record.fixes,
            "supersedes": record.supersedes,
            "lane": record.lane,
            "from": record.from,
            "after": record.after,
            "pushed": true,
        }))
        .with_change(id),
    ))
}

/// G273: an adoption's `--after` adds the changes it names to the change's dependencies, in
/// one state commit on `head`, pushed; nothing is appended when it adds none.
fn add_dependencies(
    repo: &Repo,
    request: &StartRequest,
    me: &Actor,
    change: &str,
    (chain, head): (&mut Chain, &str),
    record: &mut ChangeRecord,
) -> Result<(), String> {
    let name = change_ref(change);
    let added: Vec<String> = dedup(&request.after)
        .into_iter()
        .filter(|dependency| !record.after.contains(dependency))
        .collect();
    if !added.is_empty() {
        record.after.extend(added.iter().cloned());
        chain.sign_with(agent_signer(repo, &me.name)?);
        let draft = newest_draft(chain, change)?;
        let before = head.to_owned();
        let head = chain
            .append(
                head,
                record,
                &draft,
                &format!("{change} lands after {}", added.join(", ")),
            )
            .map_err(|error| error.message())?;
        if let Err(error) = repo
            .git
            .run(&["push", "-q", repo.remote(), &format!("{name}:{name}")])
        {
            let _ = repo.git.run(&["update-ref", &name, &before, &head]);
            return Err(format!(
                "the dependencies could not be pushed: {}",
                error.message()
            ));
        }
    }
    Ok(())
}

/// How many times an adoption reads the chain and pushes before a moving chain fails it.
const ADOPT_ATTEMPTS: usize = 3;

/// The intent already has a live change (§ 4): the same actor adopts it, unless another live
/// session of that name holds it (`CHANGE_HELD`, [`live_holder`]); anyone adopts it once it is
/// abandoned, superseded without a landing, or idle past the takeover window on this observer's
/// clock; otherwise `CHANGE_SUPERSEDED` names it. Adopting another actor's change is one more
/// live change `me` holds, so the policy's `limit` applies to it (G264). An adoption whose push
/// meets a chain a runner moved in between reads it again and adopts on it, up to
/// [`ADOPT_ATTEMPTS`] times (G402).
#[allow(clippy::too_many_lines)]
fn adopt_or_refuse(
    repo: &Repo,
    request: &StartRequest,
    me: &Actor,
    change: &str,
    base: &str,
    limit: Option<u64>,
    attempt: usize,
) -> Result<BayResult, String> {
    let name = change_ref(change);
    // G289: this store's chain moves only forward; local work the remote lacks is kept, and an
    // adoption's push carries it.
    let remote = repo.remote_chain(change).map_err(|error| error.message())?;
    let mut chain = Chain::new(&repo.git);
    let (mut head, mut record) = chain.read(change).map_err(|error| error.message())?;
    same_from(request, &record, change)?;
    // G273: a change never lands after itself.
    if let Some(why) = after_itself(repo, request, change) {
        return Err(why);
    }
    let free = record.actor.name == me.name
        || matches!(
            record.state,
            ChangeState::Abandoned | ChangeState::Superseded | ChangeState::NoEffect
        );
    if !free {
        if let Some(why) = holder_works(repo, change, &remote, &record.actor.name)? {
            return Ok(BayResult::blocked(Reason::ChangeSuperseded)
                .with_change(change)
                .with_detail(json!({"held_by": record.actor,
                    "held_by_source": record.actor_source, "why": why})));
        }
        (head, record) = chain.read(change).map_err(|error| error.message())?;
    }
    if remote_has(repo, &format!("{}{change}", crate::names::LANDED))? {
        return Ok(BayResult::blocked(Reason::ChangeAlreadyLanded).with_change(change));
    }
    // Rulings-open-questions.md Q8: another live session of the same name holds the change.
    if record.actor.name == me.name
        && !matches!(
            record.state,
            ChangeState::Abandoned | ChangeState::Superseded | ChangeState::NoEffect
        )
        && let Some(held) = live_holder(repo, change, me)?
    {
        return Ok(BayResult::blocked(Reason::ChangeHeld)
            .with_change(change)
            .with_detail(held));
    }
    if let Some(refusal) = adopt_limit(repo, base, limit, me, &record.actor.name)? {
        return Ok(refusal);
    }
    let previous = record.actor.clone();
    let previous_source = record.actor_source;
    if previous != *me {
        record.adopted_from.push(previous.clone());
        record.actor = me.clone();
        record.actor_source = Some(actor_source());
        if record.state == ChangeState::Abandoned {
            record.state = ChangeState::Open;
        }
        // § 4: adopting a chain that retention already cut re-roots it. The adoption appends a
        // live state commit to the kept head, so the chain reads its drafts again, and a later
        // landing can be cut in turn.
        record.retention_cut = false;
        chain.sign_with(agent_signer(repo, &me.name)?);
        let draft = newest_draft(&chain, change)?;
        let first_head = head.clone();
        head = chain
            .append(
                &head,
                &record,
                &draft,
                &format!("adopt {change} by {}", me.name),
            )
            .map_err(|error| error.message())?;
        if let Err(error) = repo
            .git
            .run(&["push", "-q", repo.remote(), &format!("{name}:{name}")])
        {
            let _ = repo.git.run(&["update-ref", &name, &first_head, &head]);
            let why = error.message();
            // The host judged the takeover window on its own clock and found the holder active.
            if why.contains(Reason::ChangeSuperseded.code()) {
                return Ok(BayResult::blocked(Reason::ChangeSuperseded)
                    .with_change(change)
                    .with_detail(json!({"held_by": previous,
                        "held_by_source": previous_source, "why": why})));
            }
            // A runner wrote the chain between the read and this push (a heartbeat, a receipt).
            if crate::git::ref_moved(&why) && attempt < ADOPT_ATTEMPTS {
                return adopt_or_refuse(repo, request, me, change, base, limit, attempt + 1);
            }
            return Err(format!("the adoption could not be pushed: {why}"));
        }
    }
    add_dependencies(repo, request, me, change, (&mut chain, &head), &mut record)?;
    let path = repo.view_path(change);
    if !view::exists(&path) {
        let draft = newest_draft(&chain, change)?;
        view::create(
            repo,
            change,
            &draft,
            &inputs::paths(&record.declared_inputs),
        )
        .map_err(|error| error.message())?;
    }
    let mut detail = json!({
        "view": path.display().to_string(),
        "adopted": previous != *me,
        "adopted_from": previous,
        "actor": record.actor,
        "actor_source": record.actor_source,
        "paths": request.paths,
        "after": record.after,
    });
    add_behind(repo, &mut detail, change, &record.base, base)?;
    Ok(BayResult::ok(detail).with_change(change))
}

/// G272: an adopted change's detail says how far trunk moved past its base (`behind`, trunk
/// first-parent commits in `base..trunk`) and, when it moved, how to bring that trunk in.
fn add_behind(
    repo: &Repo,
    detail: &mut serde_json::Value,
    change: &str,
    base: &str,
    trunk: &str,
) -> Result<(), String> {
    let behind: u64 = repo
        .git
        .run(&[
            "rev-list",
            "--first-parent",
            "--count",
            &format!("{base}..{trunk}"),
        ])
        .map_err(|error| error.message())?
        .trim()
        .parse()
        .map_err(|_| "git rev-list --count printed no count".to_owned())?;
    detail["behind"] = json!(behind);
    if behind > 0 {
        detail["recovery"] = json!(format!("bay start {change} brings in the newer main"));
    }
    Ok(())
}

/// G268: an outside branch opens a change of its own. A start whose `--from` is not the branch
/// the intent's live change took in (or that change took in none) never adopts it, which would
/// drop the branch; a retry of the same start adopts as any other.
fn same_from(request: &StartRequest, record: &ChangeRecord, change: &str) -> Result<(), String> {
    match &request.from {
        Some(reference)
            if record.from.as_ref().map(|from| from.reference.as_str())
                != Some(reference.as_str()) =>
        {
            Err(format!(
                "--from {reference}: the intent already has the change {change}; resume it with \
                 start {change}, or start another intent"
            ))
        }
        _ => Ok(()),
    }
}

/// Another live session of `me`'s actor name that holds `change` (§ 4, rulings-open-questions.md
/// Q8): a heartbeat in a live ref, signed by that actor, whose row of the change is another
/// session's, not `ended`, and younger than the takeover window. `CHANGE_HELD`'s detail: the
/// holder (actor and session), its machine and the heartbeat's age. The live refs are fetched
/// first; offline, the ones this repository holds are read. `me`'s own session holds nothing
/// against itself, so the holder's own `start` adopts its change.
fn live_holder(repo: &Repo, change: &str, me: &Actor) -> Result<Option<serde_json::Value>, String> {
    let live = format!("+{p}*:{p}*", p = crate::live::LIVE_PREFIX);
    let _ = repo
        .git
        .run(&["fetch", "-q", "--no-tags", repo.remote(), &live]);
    repo.fetch_registry();
    let agents = crate::live::agents_file(&repo.git)?;
    let window = repo.config.takeover().as_secs();
    let now = crate::live::unix_now();
    for (live_ref, tip) in crate::live::tips(&repo.git)? {
        let Some(holder) = crate::live::other_session_of(
            &repo.git,
            &tip,
            change,
            me,
            agents.as_deref(),
            now,
            window,
        )?
        else {
            continue;
        };
        let machine = live_ref
            .strip_prefix(crate::live::LIVE_PREFIX)
            .unwrap_or(&live_ref);
        return Ok(Some(json!({
            "holder": {"actor": me.name, "session": holder.session},
            "machine": machine,
            "heartbeat_age_s": holder.age_s,
        })));
    }
    Ok(None)
}

/// Watches the chain and the live refs for the takeover window (§ 4): a remote chain that
/// moves on from `head`, its remote head, or a new heartbeat naming the change signed by
/// `actor`, its current actor, means that actor is working, and the answer says why. A
/// heartbeat signed by anyone else is not that actor's activity, so no session keeps a dead
/// peer's change un-adoptable. The chain is read by [`Repo::remote_chain`], never over this
/// store's copy (G289).
fn holder_works(
    repo: &Repo,
    change: &str,
    head: &str,
    actor: &str,
) -> Result<Option<String>, String> {
    let window = repo.config.takeover();
    let live = format!("+{p}*:{p}*", p = crate::live::LIVE_PREFIX);
    let fetch_live = || {
        let _ = repo
            .git
            .run(&["fetch", "-q", "--no-tags", repo.remote(), &live]);
    };
    fetch_live();
    repo.fetch_registry();
    let agents = crate::live::agents_file(&repo.git)?;
    let mut tips = crate::live::tips(&repo.git)?;
    let seen = std::time::Instant::now();
    while seen.elapsed() < window {
        std::thread::sleep(std::time::Duration::from_millis(200).min(window));
        let moved = repo.remote_chain(change).is_ok_and(|remote| remote != head);
        fetch_live();
        if moved {
            return Ok(Some("its actor is working on it".to_owned()));
        }
        let now = crate::live::tips(&repo.git)?;
        for (live_ref, tip) in &now {
            if tips.get(live_ref) != Some(tip)
                && crate::live::heartbeat_of(
                    &repo.git,
                    tip,
                    change,
                    actor,
                    agents.as_deref(),
                    window.as_secs(),
                )?
            {
                return Ok(Some(format!(
                    "its actor's heartbeat on {live_ref} names it"
                )));
            }
        }
        tips = now;
    }
    Ok(None)
}

/// `start <CHANGE> --from <ref>`: the branch's net change on `trunk` becomes the change's
/// draft, appended to its chain at `head`, and its view is made again at that draft. Returns
/// the draft, or the refusal [`take_in`] gives.
fn replace_draft(
    repo: &Repo,
    chain: &Chain,
    (head, record): (&mut String, &mut ChangeRecord),
    trunk: &str,
    reference: &str,
    saved: &str,
) -> Result<Result<String, BayResult>, String> {
    let id = record.id.clone();
    let declared = inputs::paths(&record.declared_inputs);
    let taken = match take_in(repo, reference, trunk, &declared)? {
        Ok(taken) => taken,
        Err(refusal) => return Ok(Err(refusal)),
    };
    record.from = taken.from;
    trunk.clone_into(&mut record.base);
    *head = chain
        .append(
            head,
            record,
            &taken.draft,
            &format!("start --from {reference}"),
        )
        .map_err(|error| error.message())?;
    // The view's edits were checkpointed into `saved` before this; whatever is newer is kept.
    view::discard(repo, &id, saved).map_err(|error| error.message())?;
    view::create(repo, &id, &taken.draft, &declared).map_err(|error| error.message())?;
    Ok(Ok(taken.draft))
}

/// G417 (a fleet finding, 2026-10-27): `bay start` on a change already on main reopened it,
/// `bay merge` queued a new edit behind the merged revision, and that edit was refused later
/// with no word to its author. A change on main is never reopened; more work is a new change.
/// Nothing in its view is touched.
fn already_on_main(repo: &Repo, id: &str, record: &ChangeRecord) -> Option<BayResult> {
    matches!(record.state, ChangeState::Landed | ChangeState::NoEffect).then(|| {
        BayResult::blocked(Reason::ChangeAlreadyLanded)
            .with_change(id)
            .with_detail(json!({
                "state": record.state,
                "view": repo.view_path(id).display().to_string(),
                "next_step": "this change is already on main; for more edits start a new change with its own intent, and copy any new work from the view shown",
            }))
    })
}

/// Picks up change `id`; with `from`, a branch made outside bay (`start <CHANGE> --from`), its
/// net change on the newest trunk replaces the change's draft, after the view's own edits are
/// checkpointed, so a rebased branch lands under its change and never needs a new name.
#[allow(clippy::too_many_lines)]
fn resume(repo: &Repo, id: &str, from: Option<&str>) -> Result<BayResult, String> {
    if !is_change_id(id) {
        return Err(format!("{id} is not a change id"));
    }
    let mut chain = Chain::new(&repo.git);
    if chain.head(id).map_err(|error| error.message())?.is_none() {
        repo.fetch_refs(&[change_ref(id)]).map_err(|error| {
            format!(
                "change {id} is not here or on the remote: {}",
                error.message()
            )
        })?;
    }
    // The remote's chain says whether the change landed since this store last read it.
    let _ = repo.remote_chain(id);
    let (mut head, mut record) = chain.read(id).map_err(|error| error.message())?;
    if let Some(refused) = already_on_main(repo, id, &record) {
        return Ok(refused);
    }
    // G272: a change that ended without landing is never resumed behind a newer generation of
    // its intent (plain `start` adopts only the newest claim's change).
    if let Some(newer) = newer_generation(repo, &record)? {
        return Ok(superseded_by_newer(id, &record, &newer));
    }
    chain.sign_with(agent_signer(repo, &record.actor.name)?);
    let trunk = repo.fetch_trunk().map_err(|error| error.message())?;
    let latest = chain
        .drafts(id)
        .map_err(|error| error.message())?
        .first()
        .cloned()
        .ok_or("the change has no draft")?;

    let view_path = repo.view_path(id);
    let mut draft = latest.clone();
    let mut checkpointed = false;
    let complete = view::journal(repo, id).is_some_and(view::Journal::is_complete);
    let mut unsaved_kept = None;
    if view::exists(&view_path) && !complete {
        // A half-made view (start was killed mid-creation, or its journal was lost) is made
        // again; any file in it that no draft holds is kept aside, never deleted.
        unsaved_kept = view::discard(repo, id, &latest).map_err(|error| error.message())?;
    }
    if view::exists(&view_path) {
        if let Some((new_head, frozen)) =
            checkpoint_view(repo, &chain, &head, &mut record, &latest)?
        {
            head = new_head;
            draft = frozen;
            checkpointed = true;
        }
    } else {
        view::create(repo, id, &latest, &inputs::paths(&record.declared_inputs))
            .map_err(|error| error.message())?;
    }

    let old_base = record.base.clone();
    if let Some(reference) = from {
        let saved = draft.clone();
        match replace_draft(
            repo,
            &chain,
            (&mut head, &mut record),
            &trunk,
            reference,
            &saved,
        )? {
            Ok(taken) => draft = taken,
            Err(refusal) => return Ok(refusal.with_change(id)),
        }
    }
    let moved_paths = if trunk == record.base {
        Vec::new()
    } else {
        match bring_in_trunk(repo, &chain, &head, &mut record, &draft, &trunk)? {
            Ok(changed) => changed,
            Err(refusal) => return Ok(refusal),
        }
    };
    // H7, G72: the view keeps the agent product's hook-settings file, at the commit it holds now.
    view::keep_hook_settings(repo, id, &inputs::paths(&record.declared_inputs))
        .map_err(|error| error.message())?;

    // § 8: the view now holds `base`, so its versions are what the actor observed. A view left
    // on the old base (a conflict above) never counts as observing the new one.
    let observed = inputs::observe(repo, &record, &record.base);
    if observed != record.observed_versions {
        record.observed_versions = observed;
        let head = chain
            .head(id)
            .map_err(|error| error.message())?
            .ok_or("the change chain vanished")?;
        let draft = newest_draft(&chain, id)?;
        chain
            .append(&head, &record, &draft, "observe inputs")
            .map_err(|error| error.message())?;
    }

    let pushed = push_change(repo, id);
    // § 6: every resume scans base_first..trunk, so it says when the change already landed.
    let landed = crate::publish::scan(&repo.git, &record.base_first, &trunk, id)?
        .publications
        .first()
        .map(|found| json!({"commit": found.commit, "revision": found.revision}));
    Ok(BayResult::ok(json!({
        "view": view_path.display().to_string(),
        "base": record.base,
        "trunk_moved": trunk != old_base,
        "moved_paths": moved_paths,
        "checkpointed": checkpointed,
        "observed_versions": record.observed_versions,
        "pushed": pushed,
        "landed": landed,
        "unsaved_kept": unsaved_kept.map(|path| path.display().to_string()),
    }))
    .with_change(id))
}

/// The checkpoint of `record`'s view: what it observed, and its files frozen as a draft on its
/// base, appended to the chain at `head` when its tree differs from the `latest` draft. The new
/// head and draft, or `None` when the view holds nothing new.
fn checkpoint_view(
    repo: &Repo,
    chain: &Chain,
    head: &str,
    record: &mut ChangeRecord,
    latest: &str,
) -> Result<Option<(String, String)>, String> {
    let id = record.id.clone();
    let view_path = repo.view_path(&id);
    let declared = inputs::paths(&record.declared_inputs);
    view::record_observed(repo, &view_path, &declared, &mut record.observed_inputs)
        .map_err(|error| error.message())?;
    // BACKLOG 49: a checkpoint is pushed, so a large file leaves it as it leaves a merge.
    crate::assets::offload_view(repo, &view_path)?;
    if let Ok(Some(trunk)) = repo.git.ref_target(&repo.trunk_tracking_ref())
        && let Some(base) = crate::change::newer_base(&Git::new(&view_path), &record.base, &trunk)
    {
        record.base = base;
    }
    let frozen = freeze_draft(&Git::new(&view_path), &record.base, "checkpoint")
        .map_err(|error| error.message())?;
    if crate::change::same_draft(&repo.git, &frozen, latest).map_err(|error| error.message())? {
        return Ok(None);
    }
    let head = chain
        .append(head, record, &frozen, "checkpoint")
        .map_err(|error| error.message())?;
    view::set_journal(repo, &id, view::Journal::Checkpointed)?;
    Ok(Some((head, frozen)))
}

/// The checkpoint the agent hook starts (G165: at the session's end, and on the idle timer
/// `[hooks] checkpoint_idle_s`): `bay start`'s checkpoint of the view of change `id`,
/// pushed like any checkpoint, without its fetch of trunk, so the view the session works in
/// never moves under it. A change with no complete view here has nothing to checkpoint.
#[must_use]
pub fn checkpoint(dir: &Path, id: &str) -> BayResult {
    let repo = match Repo::open(dir) {
        Ok(repo) => repo,
        Err(why) => return BayResult::failed(why),
    };
    checkpoint_here(&repo, id).unwrap_or_else(BayResult::failed)
}

fn checkpoint_here(repo: &Repo, id: &str) -> Result<BayResult, String> {
    if !is_change_id(id) {
        return Err(format!("{id} is not a change id"));
    }
    let mut chain = Chain::new(&repo.git);
    let (head, mut record) = chain.read(id).map_err(|error| error.message())?;
    let view_path = repo.view_path(id);
    let complete = view::journal(repo, id).is_some_and(view::Journal::is_complete);
    if !view::exists(&view_path) || !complete {
        return Ok(BayResult::ok(json!({"checkpointed": false,
            "why": "the change has no complete view here"}))
        .with_change(id));
    }
    chain.sign_with(agent_signer(repo, &record.actor.name)?);
    let latest = chain
        .drafts(id)
        .map_err(|error| error.message())?
        .first()
        .cloned()
        .ok_or("the change has no draft")?;
    let made = checkpoint_view(repo, &chain, &head, &mut record, &latest)?;
    let pushed = push_change(repo, id);
    Ok(BayResult::ok(json!({
        "view": view_path.display().to_string(),
        "checkpointed": made.is_some(),
        "draft": made.map_or(latest, |(_, draft)| draft),
        "pushed": pushed,
    }))
    .with_change(id))
}

/// Merges the draft onto the newer trunk, moves the view there and records the new base.
/// Returns the declared paths trunk changed, or the `MERGE_CONFLICT` refusal, with the view on
/// trunk and the conflicts marked in its files.
fn bring_in_trunk(
    repo: &Repo,
    chain: &Chain,
    head: &str,
    record: &mut ChangeRecord,
    draft: &str,
    trunk: &str,
) -> Result<Result<Vec<String>, BayResult>, String> {
    let old_base = record.base.clone();
    // `--no-renames`: which paths changed is read from trees alone, never from blob contents.
    let mut args = vec![
        "diff",
        "--name-only",
        "--no-renames",
        old_base.as_str(),
        trunk,
        "--",
    ];
    let declared = inputs::paths(&record.declared_inputs);
    args.extend(declared.iter().map(String::as_str));
    let changed = repo.git.run(&args).map_err(|error| error.message())?;
    let changed: Vec<String> = changed.lines().map(str::to_owned).collect();
    let merged = merge_onto(&repo.git, trunk, draft).map_err(|error| error.message())?;
    let (tree, conflicts) = match merged {
        Merge::Clean(tree) => (tree, None),
        Merge::Conflict(paths, tree) => (tree, Some(paths)),
    };
    // A fleet finding (2026-10-26): a refresh keeps the draft's own message and author; it used
    // to replace them with "resume onto trunk".
    let message = repo
        .git
        .run(&["log", "-1", "--format=%B", draft])
        .ok()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| "resume onto trunk".to_owned());
    let rebased = crate::change::commit_on_by(&repo.git, &tree, trunk, &message, draft)
        .map_err(|error| error.message())?;
    // G212: the sparse checkout fetches only the declared files it writes.
    Git::new(repo.view_path(&record.id))
        .fetching()
        .writing_tree()
        .run(&["checkout", "-q", "-f", "--detach", &rebased])
        .map_err(|error| error.message())?;
    // BACKLOG 90: a conflict moves the view onto trunk too, with the conflict markers in the
    // listed paths, so the repair is made against main (as `bay undo` leaves its view). The
    // view's base is trunk now (G416 reads it at the next freeze); the chain keeps the last
    // draft, which holds no markers, and `bay merge` refuses one that still does.
    if let Some(paths) = conflicts {
        return Ok(Err(BayResult::blocked(Reason::MergeConflict)
            .with_change(record.id.clone())
            .with_detail(json!({
                "conflicts": paths,
                "trunk": trunk,
                "base": old_base,
                "view": repo.view_path(&record.id).display().to_string(),
                "next": "the view is on main now, with conflict markers in the listed paths: \
                    resolve them there, then bay merge",
            }))));
    }
    trunk.clone_into(&mut record.base);
    trunk.clone_into(&mut record.observed_remote);
    chain
        .append(head, record, &rebased, "resume: trunk moved")
        .map_err(|error| error.message())?;
    Ok(Ok(changed))
}

/// The newest draft of change `id` ([`Chain::latest_draft`]).
fn newest_draft(chain: &Chain, id: &str) -> Result<String, String> {
    chain.latest_draft(id).map_err(|error| error.message())
}

/// `after` in the order given, each change once.
fn dedup(after: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for change in after {
        if !out.contains(change) {
            out.push(change.clone());
        }
    }
    out
}

/// The agent's signer when the repository has a key registry (§ 10a): this machine's agent
/// key under the actor's name. Without a registry, or under `[policy] signing = "approvals"`
/// (N-21), bay runs unsigned.
fn agent_signer(repo: &Repo, actor: &str) -> Result<Option<Signer>, String> {
    repo.fetch_registry();
    if Registry::current(&repo.git)?
        .as_ref()
        .is_none_or(|registry| !registry.signs_all())
    {
        return Ok(None);
    }
    let key = Keys::load(&repo.git).agent.ok_or(
        "the repository has a key registry and this machine has no agent key (git config bay.agentKey)",
    )?;
    Ok(Some(Signer {
        key,
        name: actor.to_owned(),
    }))
}

/// Pushes the change chain. A chain only appends, so this is a fast-forward; a refusal or an
/// unreachable remote leaves the chain local and is reported, never forced.
fn push_change(repo: &Repo, id: &str) -> bool {
    let name = change_ref(id);
    repo.git
        .run(&["push", "-q", repo.remote(), &format!("{name}:{name}")])
        .is_ok()
}

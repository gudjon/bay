//! `bay start`: open a change, or resume one (`DESIGN` § 4).
//!
//! Built here (increment 1, basic): a fresh fetch of trunk, a sparse view of the declared
//! paths, the change record and its chain, pushed to the remote; `--resume` checkpoints the
//! draft, brings in a newer trunk and reports what moved. Intent claims and adoption are
//! kickstart item 19e; the view journal is item 7; the shared machine cache is item 15.

use std::path::Path;

use serde_json::json;

use crate::change::{
    Actor, Chain, ChangeRecord, ChangeState, Intent, IntentClaim, RECORD_SCHEMA, actor_source,
    change_ref, commit_on, current_actor, freeze_draft, intent_hash,
};
use crate::git::Git;
use crate::ids::{is_change_id, new_change_id};
use crate::inputs::{self, DeclaredInput, InputName};
use crate::merge::{Merge, merge_onto};
use crate::registry::Registry;
use crate::repo::Repo;
use crate::result::{BayResult, Reason};
use crate::signing::{Keys, Signer};
use crate::view;

pub struct StartRequest {
    pub intent: Option<String>,
    /// Claim the intent's next generation after its change landed (`start --new`).
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
}

/// How long a start whose claim push met a held remote ref lock waits before it re-reads.
const CLAIM_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

#[must_use]
pub fn start(dir: &Path, request: &StartRequest) -> BayResult {
    let repo = match Repo::open_fetched(dir) {
        Ok(repo) => repo,
        Err(error) => return error.into_result(),
    };
    let result = match &request.resume {
        Some(id) => resume(&repo, id),
        None => open(&repo, request),
    };
    let mut result = result.unwrap_or_else(BayResult::failed);
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
        let ended = matches!(
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
        })
    };
    view::release_ended(repo, repo.config.retention_window(), &ended)
}

fn open(repo: &Repo, request: &StartRequest) -> Result<BayResult, String> {
    let intent = request
        .intent
        .clone()
        .ok_or("bay start needs an intent, or --resume <change>")?;
    let base = repo.fetch_trunk().map_err(|error| error.message())?;
    let me = current_actor()?;
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
    // G161: a fix names a change on trunk, or nothing is created.
    if let Some(fixed) = &request.fixes {
        if !is_change_id(fixed) {
            return Err(format!("--fixes {fixed} is not a change id"));
        }
        if crate::undo::landed_commit(repo, fixed, &base)?.is_none() {
            return Ok(BayResult::blocked(Reason::ChangeNotLanded)
                .with_change(fixed.clone())
                .with_detail(json!({"trunk": base, "fixes": fixed,
                    "why": "a fix names a change that landed"})));
        }
    }
    let hash = intent_hash(&intent, &request.intent_revision);
    // § 4: one intent, one change, by claim. A lost race re-reads the claims.
    for _ in 0..5 {
        let claims = remote_claims(repo, &hash)?;
        let (generation, supersedes) = match claims.last_key_value() {
            None => (1, None),
            Some((generation, change)) => {
                let landed = remote_has(repo, &format!("{}{change}", crate::names::LANDED))?;
                match (landed, request.new) {
                    (true, true) => (generation + 1, Some(change.clone())),
                    (true, false) => {
                        return Ok(BayResult::blocked(Reason::ChangeAlreadyLanded)
                            .with_change(change.clone())
                            .with_detail(json!({"intent": intent, "generation": generation,
                                "next": "start --new claims the next generation"})));
                    }
                    (false, true) => {
                        return Ok(BayResult::blocked(Reason::ChangeSuperseded)
                            .with_change(change.clone())
                            .with_detail(json!({"intent": intent, "generation": generation,
                                "why": "the next generation needs this one landed"})));
                    }
                    (false, false) => return adopt_or_refuse(repo, request, &me, change),
                }
            }
        };
        let claim = IntentClaim {
            hash: hash.clone(),
            generation,
        };
        if let Some(result) = create(repo, request, &intent, &me, &base, claim, supersedes)? {
            return Ok(result);
        }
    }
    Err(format!("the claim on {intent} kept moving; try again"))
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
                "{} is compacted without its landed claim",
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
                "the publication {landed} of {} names no one change",
                claim.landed_ref()
            ));
        }
        claims.insert(generation, change.to_owned());
    }
    Ok(())
}

/// Creates the change and its claim in one atomic push (§ 4), so a crash never leaves a claim
/// without a change. Returns `None` when another start claimed the generation first.
fn create(
    repo: &Repo,
    request: &StartRequest,
    intent: &str,
    me: &Actor,
    base: &str,
    claim: IntentClaim,
    supersedes: Option<String>,
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
    };
    // § 8: the view is of `base`, so what the actor reads there is what it observed.
    record.observed_versions = inputs::observe(repo, &record, base);
    let mut chain = Chain::new(&repo.git);
    chain.sign_with(agent_signer(repo, &me.name)?);
    let head = chain
        .create(&record, base, &format!("start {intent}"))
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
    let view = view::create(repo, &id, base, &request.paths).map_err(|error| error.message())?;
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
            "pushed": true,
        }))
        .with_change(id),
    ))
}

/// The intent already has a live change (§ 4): the same actor adopts it, unless another live
/// session of that name holds it (`CHANGE_HELD`, [`live_holder`]); anyone adopts it once it is
/// abandoned, superseded without a landing, or idle past the takeover window on this observer's
/// clock; otherwise `CHANGE_SUPERSEDED` names it.
fn adopt_or_refuse(
    repo: &Repo,
    request: &StartRequest,
    me: &Actor,
    change: &str,
) -> Result<BayResult, String> {
    let name = change_ref(change);
    repo.git
        .run(&[
            "fetch",
            "-q",
            "--no-tags",
            repo.remote(),
            &format!("+{name}:{name}"),
        ])
        .map_err(|error| error.message())?;
    let mut chain = Chain::new(&repo.git);
    let (mut head, mut record) = chain.read(change).map_err(|error| error.message())?;
    let free = record.actor.name == me.name
        || matches!(
            record.state,
            ChangeState::Abandoned | ChangeState::Superseded | ChangeState::NoEffect
        );
    if !free {
        if let Some(why) = holder_works(repo, &chain, change, &head, &record.actor.name)? {
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
        let draft = chain.drafts(change).map_err(|error| error.message())?[0].clone();
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
            return Err(format!("the adoption could not be pushed: {why}"));
        }
    }
    let _ = head;
    let path = repo.view_path(change);
    if !view::exists(&path) {
        let draft = chain.drafts(change).map_err(|error| error.message())?[0].clone();
        view::create(
            repo,
            change,
            &draft,
            &inputs::paths(&record.declared_inputs),
        )
        .map_err(|error| error.message())?;
    }
    Ok(BayResult::ok(json!({
        "view": path.display().to_string(),
        "adopted": previous != *me,
        "adopted_from": previous,
        "actor": record.actor,
        "actor_source": record.actor_source,
        "paths": request.paths,
    }))
    .with_change(change))
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

/// Watches the chain and the live refs for the takeover window (§ 4): a chain that moves, or a
/// new heartbeat naming the change signed by `actor`, its current actor, means that actor is
/// working, and the answer says why. A heartbeat signed by anyone else is not that actor's
/// activity, so no session keeps a dead peer's change un-adoptable.
fn holder_works(
    repo: &Repo,
    chain: &Chain,
    change: &str,
    head: &str,
    actor: &str,
) -> Result<Option<String>, String> {
    let name = change_ref(change);
    let window = repo.config.takeover();
    let live = format!("+{p}*:{p}*", p = crate::live::LIVE_PREFIX);
    let fetch = || {
        let _ = repo.git.run(&[
            "fetch",
            "-q",
            "--no-tags",
            repo.remote(),
            &format!("+{name}:{name}"),
            &live,
        ]);
    };
    fetch();
    repo.fetch_registry();
    let agents = crate::live::agents_file(&repo.git)?;
    let mut tips = crate::live::tips(&repo.git)?;
    let seen = std::time::Instant::now();
    while seen.elapsed() < window {
        std::thread::sleep(std::time::Duration::from_millis(200).min(window));
        fetch();
        if chain
            .head(change)
            .map_err(|error| error.message())?
            .as_deref()
            != Some(head)
        {
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

fn resume(repo: &Repo, id: &str) -> Result<BayResult, String> {
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
    let (mut head, mut record) = chain.read(id).map_err(|error| error.message())?;
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
    if view::exists(&view_path) && !complete {
        // A half-made view (start was killed mid-creation) is released and made again.
        view::discard(repo, id).map_err(|error| error.message())?;
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
    let moved_paths = if trunk == old_base {
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
        let draft = chain.drafts(id).map_err(|error| error.message())?[0].clone();
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
    let frozen = freeze_draft(&Git::new(&view_path), &record.base, "checkpoint")
        .map_err(|error| error.message())?;
    if tree_of(repo, &frozen)? == tree_of(repo, latest)? {
        return Ok(None);
    }
    let head = chain
        .append(head, record, &frozen, "checkpoint")
        .map_err(|error| error.message())?;
    view::set_journal(repo, &id, view::Journal::Checkpointed)?;
    Ok(Some((head, frozen)))
}

/// The checkpoint the agent hook starts (G165: at the session's end, and on the idle timer
/// `[hooks] checkpoint_idle_s`): `bay start --resume`'s checkpoint of the view of change `id`,
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
/// Returns the declared paths trunk changed, or the `MERGE_CONFLICT` refusal.
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
    match merge_onto(&repo.git, trunk, draft).map_err(|error| error.message())? {
        Merge::Conflict(paths, _) => Ok(Err(BayResult::blocked(Reason::MergeConflict)
            .with_change(record.id.clone())
            .with_detail(json!({"conflicts": paths, "trunk": trunk, "base": old_base})))),
        Merge::Clean(tree) => {
            let rebased = commit_on(&repo.git, &tree, trunk, "resume onto trunk")
                .map_err(|error| error.message())?;
            // G212: the sparse checkout fetches only the declared files it writes.
            Git::new(repo.view_path(&record.id))
                .fetching()
                .run(&["checkout", "-q", "-f", "--detach", &rebased])
                .map_err(|error| error.message())?;
            trunk.clone_into(&mut record.base);
            trunk.clone_into(&mut record.observed_remote);
            chain
                .append(head, record, &rebased, "resume: trunk moved")
                .map_err(|error| error.message())?;
            Ok(Ok(changed))
        }
    }
}

/// The agent's signer when the repository has a key registry (§ 10a): this machine's agent
/// key under the actor's name. Without a registry bay runs unsigned.
fn agent_signer(repo: &Repo, actor: &str) -> Result<Option<Signer>, String> {
    repo.fetch_registry();
    if Registry::current(&repo.git)?.is_none() {
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

fn tree_of(repo: &Repo, commit: &str) -> Result<String, String> {
    repo.git
        .run(&["rev-parse", &format!("{commit}^{{tree}}")])
        .map_err(|error| error.message())
}

/// Pushes the change chain. A chain only appends, so this is a fast-forward; a refusal or an
/// unreachable remote leaves the chain local and is reported, never forced.
fn push_change(repo: &Repo, id: &str) -> bool {
    let name = change_ref(id);
    repo.git
        .run(&["push", "-q", repo.remote(), &format!("{name}:{name}")])
        .is_ok()
}

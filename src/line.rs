//! `bay queue`: the live line, head first, for a person or a program (a fleet ask, 2026-10-29:
//! a tool beside bay, as lazygit is beside Git, needs the line in one fast read, never a parse
//! of `bay status`).
//!
//! It reads only what the line holds now: the reservations with no release ([`Queue::live`]),
//! and the change records of those places, fetched in one batch ([`Repo::remote_chains`]) and
//! read in one `cat-file --batch`. A change is placed by a runner, so `arriving` lists the
//! changes submitted that no runner has placed yet: with no runner running, they are the whole
//! line. A change a runner took with no place, straight off an empty line, is `taken`: the
//! holders' leases name it (G853). No landed change and no evidence is read, so a store with
//! thousands of places in its
//! history answers as fast as one with a few. It writes nothing but fetched copies of the
//! remote's refs and the merge requests it found answered ([`crate::answered`]); a store read
//! past its sweep's window starts the status sweep, which refreshes the store's copy for every
//! read after it. A tool changes the line only through bay's own verbs (`bay merge`,
//! `bay abandon`, `bay start <change>`).
//!
//! `detail` is one `bay.queue/1` document ([`schema`], also in `bay help --json`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde_json::{Value, json};

use crate::change::{
    Chain, ChangeRecord, ChangeState, OpState, Operation, RECORD_FILE, change_ref,
};
use crate::names::{CHANGES, REQUESTS};
use crate::queue::{Place, Queue};
use crate::repo::Repo;
use crate::result::BayResult;

/// The schema name of `bay queue`'s `detail`.
pub const SCHEMA: &str = "bay.queue/1";

/// `bay queue`.
#[must_use]
pub fn queue(dir: &Path, from_store: bool) -> BayResult {
    let repo = match Repo::open(dir) {
        Ok(repo) => repo,
        Err(error) => return BayResult::failed(error),
    };
    let mut result = match read(&repo, from_store) {
        Ok(mut detail) => {
            // G837 (a fleet finding, 2026-11-21: a panel that read the line from the store every
            // 15 s found its copy 106 s old, as only a status read swept the store): a store read
            // past its last sweep's window starts the sweep once it has read, and leaves it
            // running, as a store read of `bay status` does (G710); `sweep` says so, and is null
            // when this read started none.
            let closed = from_store
                && !crate::sweep::open_by_time(&repo.common_dir, crate::live::unix_now());
            detail["sweep"] = if closed {
                crate::sweeper::leave_running(dir)
            } else {
                Value::Null
            };
            BayResult::ok(detail)
        }
        Err(error) => BayResult::failed(error),
    };
    result.freshness = repo.freshness();
    result
}

/// The `bay.queue/1` document of the line as the remote holds it now. G756 (owner,
/// 2026-11-16: a panel waited 23 s for the line while bay's own status answered in 0.03 s):
/// `from_store` reads this machine's copy of the line and of its changes, with no call to the
/// remote, and says so in `from_store`; how old that copy is, `freshness` names.
fn read(repo: &Repo, from_store: bool) -> Result<Value, String> {
    let mut unread: Vec<Value> = Vec::new();
    // G785: a live read asks the remote twice: one listing, one fetch of what moved.
    let line = if from_store {
        None
    } else {
        Line::read(repo, &mut unread)
    };
    let places = if from_store || line.is_some() {
        Queue::live_here(repo)?
    } else {
        Queue::live(repo)?
    };
    let mut ids: Vec<String> = places
        .iter()
        .map(|place| place.entry.change.clone())
        .filter(|change| !change.is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    if !from_store {
        // A place whose chain the one fetch did not settle (no request listed for it, or no
        // listing at all) is fetched as before.
        let rest: Vec<String> = match &line {
            Some(line) => ids
                .iter()
                .filter(|change| !line.settled.contains(*change))
                .cloned()
                .collect(),
            None => ids.clone(),
        };
        unread.extend(
            repo.remote_chains(&rest)
                .into_iter()
                .map(|(change, why)| json!({"change": change, "why": why})),
        );
    }
    let records = records(repo, &ids);
    let now = crate::live::unix_now();
    let rows: Vec<Value> = places
        .iter()
        .enumerate()
        .map(|(at, place)| {
            let record = records
                .iter()
                .find(|record| record.id == place.entry.change);
            if record.is_none()
                && !unread
                    .iter()
                    .any(|row| row["change"] == place.entry.change.as_str())
            {
                unread.push(json!({"change": place.entry.change, "why": "no change record here"}));
            }
            row(at + 1, place, record, now)
        })
        .collect();
    let in_line: BTreeSet<&str> = places
        .iter()
        .map(|place| place.entry.change.as_str())
        .collect();
    // G830 (a fleet finding, 2026-11-20: a panel read of the line said "Nothing waits" while six
    // changes had waited for a runner up to 64 min): a store read takes the arrivals from this
    // store's copy of the remote's merge requests, as its last sweep left them, and none when no
    // sweep ever ran here.
    let swept = if from_store {
        crate::sweep::last(&repo.common_dir).map(|(_, at)| at)
    } else {
        None
    };
    let arriving = match (from_store, swept) {
        (false, _) => arriving(
            repo,
            &in_line,
            Listing::Live(line.as_ref()),
            now,
            &mut unread,
        )?,
        (true, Some(_)) => arriving(
            repo,
            &in_line,
            Listing::Stored(&stored_requests(repo)?),
            now,
            &mut unread,
        )?,
        (true, None) => Vec::new(),
    };
    let taken = taken(repo, &in_line, now, &mut unread);
    let oldest = oldest(&places, &arriving, &taken, now);
    Ok(json!({
        "schema": SCHEMA,
        "read_at": crate::clock::rfc3339_of(now),
        "depth": rows.len(),
        "oldest_wait_s": oldest,
        "places": rows,
        "arriving": arriving,
        "taken": taken,
        // G831 (a fleet finding, 2026-11-21: a panel could not tell a store read that lists the
        // arrivals from one of an older bay that lists none, and a store no sweep ever ran in
        // said "Nothing waits"): whether `arriving` was read at all.
        "arrivals_listed": !from_store || swept.is_some(),
        "unread": unread,
        "from_store": from_store,
        "age_s": if from_store { store_age(repo, swept, now) } else { json!(0) },
    }))
}

/// G765: how old this store's copy of the line is; null when it never fetched it here. G830: its
/// arrivals are as old as the store's last sweep, so the copy is no newer than that sweep.
fn store_age(repo: &Repo, swept: Option<u64>, now: u64) -> Value {
    crate::queue::fetched_age_s(&repo.common_dir).map_or(Value::Null, |age| {
        json!(age.max(swept.map_or(0, |at| now.saturating_sub(at))))
    })
}

/// G830: this store's copy of the remote's merge requests, printed as `ls-remote` prints them.
fn stored_requests(repo: &Repo) -> Result<String, String> {
    repo.git
        .run(&[
            "for-each-ref",
            "--format=%(objectname)%09%(refname)",
            REQUESTS,
        ])
        .map_err(|error| {
            format!(
                "this store's merge requests could not be listed: {}",
                error.message()
            )
        })
}

/// Where the arrivals' list of merge requests comes from.
#[derive(Clone, Copy)]
enum Listing<'a> {
    /// A live read: the line's one listing, or with none an `ls-remote` of its own; then one
    /// fetch of each requested chain the listing did not settle.
    Live(Option<&'a Line>),
    /// A store read (G830): this store's copy of the requests, as `ls-remote` prints them, and
    /// the chains it holds; nothing is fetched.
    Stored(&'a str),
}

/// The listing of the merge requests `listing` names, as `ls-remote` prints it.
fn requests_listed(repo: &Repo, listing: Listing<'_>) -> Result<String, String> {
    match listing {
        Listing::Stored(listed) => Ok(listed.to_owned()),
        Listing::Live(Some(line)) => Ok(line.requests.clone()),
        Listing::Live(None) => repo
            .git
            .run(&["ls-remote", repo.remote(), &format!("{REQUESTS}*")])
            .map_err(|error| {
                format!(
                    "the remote's merge requests could not be listed: {}",
                    error.message()
                )
            }),
    }
}

/// The changes submitted that no runner has placed yet, oldest request first. One `ls-remote`
/// lists the remote's merge requests, as a runner's arrivals do (S1). A change whose local
/// record already shows that request's operation ended or past `submitted` is passed over
/// unread; the rest are fetched in one batch and read in one.
fn arriving(
    repo: &Repo,
    placed: &BTreeSet<&str>,
    listing: Listing<'_>,
    now: u64,
    unread: &mut Vec<Value>,
) -> Result<Vec<Value>, String> {
    let listed = requests_listed(repo, listing)?;
    let requests = unanswered(
        repo,
        listed
            .lines()
            .filter_map(|line| {
                let (request, name) = line.split_once('\t')?;
                let change = name.strip_prefix(REQUESTS)?;
                (!placed.contains(change)).then(|| (change.to_owned(), request.to_owned()))
            })
            .collect(),
    );
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let changes: Vec<String> = requests.iter().map(|(change, _)| change.clone()).collect();
    // G785: the listing's one fetch settled every requested chain that moved; the rest are
    // current here already. Only one it could not settle is fetched again.
    let unsettled: Vec<String> = match listing {
        Listing::Stored(_) => Vec::new(),
        Listing::Live(Some(line)) => changes
            .iter()
            .filter(|change| !line.settled.contains(*change) && !line.ended.contains(*change))
            .cloned()
            .collect(),
        Listing::Live(None) => changes.clone(),
    };
    for (change, why) in repo.remote_chains(&unsettled) {
        unread.push(json!({"change": change, "why": why}));
    }
    let fresh = records(repo, &changes);
    let chain = Chain::new(&repo.git);
    let mut waiting: Vec<(&ChangeRecord, &Operation)> = Vec::new();
    for (change, request) in &requests {
        let Some(record) = fresh.iter().find(|record| &record.id == change) else {
            // G830: a store read fetches no chain; one its last sweep did not keep is unread.
            if matches!(listing, Listing::Stored(_)) {
                unread.push(json!({"change": change, "why": "this store holds no chain of it"}));
            }
            continue;
        };
        if passed_over(record, request) {
            continue;
        }
        match op_of(record, request) {
            Some(op) => waiting.push((record, op)),
            None => unread.push(json!({
                "change": change,
                "why": "its merge request is not in its record here",
            })),
        }
    }
    let times = commit_times(
        repo,
        &waiting
            .iter()
            .filter_map(|(_, op)| op.request_id.as_deref())
            .collect::<Vec<_>>(),
    );
    let mut rows: Vec<(Option<u64>, Value)> = waiting
        .into_iter()
        .map(|(record, op)| {
            let at = op
                .request_id
                .as_deref()
                .and_then(|request| times.get(request).copied());
            let row = json!({
                "change": record.id,
                "op_id": op.op_id,
                "intent": record.intent.reference,
                "actor": record.actor.name,
                "paths": crate::inputs::paths(&record.declared_inputs),
                "fixes": record.fixes,
                "after": record.after,
                "outcome": op.outcome,
                // G776: the changes it names that are not on main yet here, which hold it.
                "held_by": record.after.iter().filter(|change| {
                    chain.read(change).map_or(true, |(_, named)| named.state != ChangeState::Landed)
                }).collect::<Vec<_>>(),
                "requested_at": at.map(crate::clock::rfc3339_of),
                "waited_s": at.map(|at| now.saturating_sub(at)),
            });
            (at, row)
        })
        .collect();
    rows.sort_by_key(|(at, row)| (at.unwrap_or(u64::MAX), row["change"].to_string()));
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}

/// The requests of `listed`, each `(change, request)`, that the record at their change's chain
/// head here does not show answered. G835: the record of a pair an earlier read kept answered
/// ([`crate::answered`]) is not read again; a change with no chain here is kept, for the read
/// that follows to settle.
fn unanswered(repo: &Repo, listed: Vec<(String, String)>) -> Vec<(String, String)> {
    let chain = Chain::new(&repo.git);
    let heads = chain.heads();
    let store = repo.git.answer_store();
    let kept = store
        .as_deref()
        .map(crate::answered::kept)
        .unwrap_or_default();
    let pair = |change: &str, request: &str| {
        heads
            .get(change)
            .map(|head| (head.clone(), request.to_owned()))
    };
    let unknown: Vec<(&str, crate::answered::Pair)> = listed
        .iter()
        .filter_map(|(change, request)| Some((change.as_str(), pair(change, request)?)))
        .filter(|(_, pair)| !kept.contains(pair))
        .collect();
    let at: Vec<&str> = unknown.iter().map(|(_, (head, _))| head.as_str()).collect();
    let records = chain.records_at(&at).unwrap_or_default();
    let answered: BTreeSet<&crate::answered::Pair> = unknown
        .iter()
        .zip(records)
        .filter(|((change, (_, request)), record)| {
            record
                .as_ref()
                .is_some_and(|record| record.id == *change && passed_over(record, request))
        })
        .map(|((_, pair), _)| pair)
        .collect();
    let keep: BTreeSet<crate::answered::Pair> = listed
        .iter()
        .filter_map(|(change, request)| pair(change, request))
        .filter(|pair| kept.contains(pair) || answered.contains(pair))
        .collect();
    if let Some(store) = store.as_deref().filter(|_| keep != kept) {
        crate::answered::keep(store, &keep);
    }
    listed
        .into_iter()
        .filter(|(change, request)| pair(change, request).is_none_or(|pair| !keep.contains(&pair)))
        .collect()
}

/// The operation of `record` that carries merge request `request`.
fn op_of<'a>(record: &'a ChangeRecord, request: &str) -> Option<&'a Operation> {
    record
        .operations
        .iter()
        .find(|op| op.request_id.as_deref() == Some(request))
}

/// Whether `record` still waits for a runner on `request`: `None` when the record does not
/// carry that request. G597: a landed change waits for nothing, also once the retention cut
/// dropped the operation its request ref still names, so this clone's record spares the fetch
/// of its chain. An abandoned change may be adopted and asked again, so only the remote's record
/// says that one waits for nothing.
///
/// G692 (a fleet finding, 2026-11-10: `bay queue` said the line's longest wait was 1,414 minutes,
/// for a later revision of a change that had merged; no runner runs one, as its change has ended):
/// a change with a published operation waits for nothing either, whatever its state says.
fn waits(record: &ChangeRecord, request: &str) -> Option<bool> {
    let published = record
        .operations
        .iter()
        .any(|op| op.state == OpState::Published);
    if record.state == ChangeState::Landed || published {
        return Some(false);
    }
    let op = op_of(record, request)?;
    Some(!crate::status::ended(record.state) && op.state == OpState::Submitted)
}

/// Whether the line passes over `request` by `record`: its change waits for nothing.
///
/// G597 (2026-10-31): the retention cut of an ended change keeps none of its operations, while
/// the request ref it answered stays on the remote; that change waits for nothing either.
fn passed_over(record: &ChangeRecord, request: &str) -> bool {
    match waits(record, request) {
        Some(waits) => !waits,
        None => record.retention_cut && crate::status::ended(record.state),
    }
}

/// The committer time of each of `commits`, read in one `cat-file --batch`. G855: never by
/// `git show`, which runs its diff on each commit; a request commit's tree differs from its
/// parent's in every path the change touches, so one store read cost about 1 s and 400 MB.
fn commit_times(repo: &Repo, commits: &[&str]) -> HashMap<String, u64> {
    let commits: Vec<String> = commits.iter().map(|commit| (*commit).to_owned()).collect();
    crate::flow::commit_times(&repo.git, &commits).unwrap_or_default()
}

/// The records of `ids`, read in one `cat-file --batch` of their chain heads; a record the
/// batch cannot give is read on its own, and one that cannot be read is left out (its place
/// shows `state` `unread`).
fn records(repo: &Repo, ids: &[String]) -> Vec<ChangeRecord> {
    let names: Vec<String> = ids
        .iter()
        .map(|id| format!("{}:{RECORD_FILE}", change_ref(id)))
        .collect();
    let texts = repo.git.objects(&names).unwrap_or_default();
    let chain = Chain::new(&repo.git);
    ids.iter()
        .enumerate()
        .filter_map(|(at, id)| {
            let parsed = texts
                .get(at)
                .cloned()
                .flatten()
                .and_then(|text| serde_json::from_slice::<ChangeRecord>(&text).ok())
                .filter(|record| &record.id == id);
            parsed.or_else(|| chain.read(id).ok().map(|(_, record)| record))
        })
        .collect()
}

/// G853 (a fleet finding, 2026-11-22: `bay queue` said "Nothing waits to be added to main." for
/// 17 minutes while a runner checked a change it had taken off an empty line): the changes a
/// runner checks under its lease with no place in the line, oldest request first, as this store's
/// copy of the leases names them. A live read fetched them with the line; a store read has them as
/// its last sweep left them. A lease counts only while the change's record here names its holder
/// ([`crate::lease::Lease::holds`]), so a released or overtaken lease is never shown.
fn taken(repo: &Repo, placed: &BTreeSet<&str>, now: u64, unread: &mut Vec<Value>) -> Vec<Value> {
    let prefix = crate::lease::LEASE_PREFIX;
    let listed = repo
        .git
        .run(&[
            "for-each-ref",
            "--format=%(objectname) %(committerdate:unix) %(refname)",
            prefix,
        ])
        .unwrap_or_default();
    let leases: Vec<(crate::lease::Lease, u64)> = listed
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, ' ');
            let (commit, beat, name) = (fields.next()?, fields.next()?, fields.next()?);
            let change = name.strip_prefix(prefix)?;
            if placed.contains(change) {
                return None;
            }
            let lease = crate::lease::read(&repo.git, commit).ok().flatten()?;
            (lease.change == change).then_some((lease, beat.parse().ok()?))
        })
        .collect();
    let ids: Vec<String> = leases
        .iter()
        .map(|(lease, _)| lease.change.clone())
        .collect();
    let records = records(repo, &ids);
    let mut held: Vec<(&crate::lease::Lease, u64, &ChangeRecord, &Operation)> = Vec::new();
    for (lease, beat) in &leases {
        let Some(record) = records.iter().find(|record| record.id == lease.change) else {
            unread.push(json!({
                "change": lease.change,
                "why": "a runner's lease names it, and its record is not here",
            }));
            continue;
        };
        let op = record.operations.iter().find(|op| op.op_id == lease.op_id);
        if let Some(op) = op.filter(|op| lease.holds(op) && !crate::status::ended(record.state)) {
            held.push((lease, *beat, record, op));
        }
    }
    let times = commit_times(
        repo,
        &held
            .iter()
            .filter_map(|(_, _, _, op)| op.request_id.as_deref())
            .collect::<Vec<_>>(),
    );
    let mut rows: Vec<(Option<u64>, Value)> = held
        .into_iter()
        .map(|(lease, beat, record, op)| {
            let at = op
                .request_id
                .as_deref()
                .and_then(|request| times.get(request).copied());
            let row = json!({
                "change": record.id,
                "op_id": op.op_id,
                "intent": record.intent.reference,
                "actor": record.actor.name,
                "paths": crate::inputs::paths(&record.declared_inputs),
                "runner": lease.holder,
                "state": op.state,
                // G845: the operations its check covers, its own first; null while it plans.
                "checks": lease.batch,
                "beat_at": crate::clock::rfc3339_of(beat),
                "requested_at": at.map(crate::clock::rfc3339_of),
                "waited_s": at.map(|at| now.saturating_sub(at)),
            });
            (at, row)
        })
        .collect();
    rows.sort_by_key(|(at, row)| (at.unwrap_or(u64::MAX), row["change"].to_string()));
    rows.into_iter().map(|(_, row)| row).collect()
}

/// One place of the line: where it stands, the change and its operation as its record says.
fn row(place: usize, at: &Place, record: Option<&ChangeRecord>, now: u64) -> Value {
    let entry = &at.entry;
    let op = record.and_then(|record| record.operations.iter().find(|op| op.op_id == entry.op_id));
    json!({
        "place": place,
        "seq": entry.seq,
        "change": entry.change,
        "op_id": entry.op_id,
        "intent": record.map(|record| record.intent.reference.as_str()),
        "actor": record.map(|record| record.actor.name.as_str()),
        "paths": record.map(|record| crate::inputs::paths(&record.declared_inputs)),
        "fixes": record.and_then(|record| record.fixes.as_deref()),
        "after": record.map(|record| record.after.as_slice()),
        "joined_at": crate::clock::rfc3339_of(at.entry.joined),
        "waited_s": now.saturating_sub(at.entry.joined),
        "expedite": entry.expedite,
        "state": op.map_or_else(
            || Value::from("unread"),
            |op| serde_json::to_value(op.state).unwrap_or(Value::Null),
        ),
        "attempts": op.map(|op| op.attempts.len()),
        "runner": op.and_then(|op| op.owner.as_deref()),
        "outcome": op.and_then(|op| op.outcome.as_deref()),
    })
}

/// The longest wait of the line, in seconds: of its places, its arrivals and the changes runners
/// took with no place. G776 (a fleet finding, 2026-11-17: the line read "the longest has waited
/// 171 minutes" while no place had waited 12): a change held until another merges (`--after`)
/// waits for that change, not for the line, so its wait is not the line's.
fn oldest(places: &[Place], arriving: &[Value], taken: &[Value], now: u64) -> Option<u64> {
    places
        .iter()
        .map(|place| now.saturating_sub(place.entry.joined))
        .chain(
            arriving
                .iter()
                .filter(|row| row["held_by"].as_array().is_none_or(Vec::is_empty))
                .filter_map(|row| row["waited_s"].as_u64()),
        )
        .chain(taken.iter().filter_map(|row| row["waited_s"].as_u64()))
        .max()
}

/// The JSON Schema of a `taken` row of `bay.queue/1` (G853): a change a runner checks with no
/// place in the line.
fn taken_schema() -> Value {
    let string = json!({"type": "string"});
    let count = json!({"type": "integer", "minimum": 0});
    let maybe = |kind: Value| json!({"anyOf": [kind, {"type": "null"}]});
    let strings = json!({"type": "array", "items": string});
    json!({
        "type": "object",
        "required": [
            "change", "op_id", "intent", "actor", "paths", "runner", "state", "checks",
            "beat_at", "requested_at", "waited_s",
        ],
        "additionalProperties": false,
        "properties": {
            "change": string,
            "op_id": string,
            "intent": string,
            "actor": string,
            "paths": strings.clone(),
            "runner": string,
            "state": {"enum": ["running", "checked"]},
            "checks": maybe(strings),
            "beat_at": string,
            "requested_at": maybe(string.clone()),
            "waited_s": maybe(count),
        },
    })
}

/// The JSON Schema (draft 2020-12) of `bay.queue/1`, exported by `bay help --json`.
#[must_use]
pub fn schema() -> Value {
    let string = json!({"type": "string"});
    let count = json!({"type": "integer", "minimum": 0});
    let maybe = |kind: Value| json!({"anyOf": [kind, {"type": "null"}]});
    let strings = json!({"type": "array", "items": string});
    let place = json!({
        "type": "object",
        "required": [
            "place", "seq", "change", "op_id", "intent", "actor", "paths", "fixes",
            "after", "joined_at", "waited_s", "expedite", "state", "attempts", "runner",
            "outcome",
        ],
        "additionalProperties": false,
        "properties": {
            "place": {"type": "integer", "minimum": 1},
            "seq": count,
            "change": string,
            "op_id": string,
            "intent": maybe(string.clone()),
            "actor": maybe(string.clone()),
            "paths": maybe(strings.clone()),
            "fixes": maybe(string.clone()),
            "after": maybe(strings.clone()),
            "joined_at": string,
            "waited_s": count,
            "expedite": {"type": "boolean"},
            "state": {"enum": [
                "submitted", "admitted", "running", "checked", "published",
                "blocked", "failed", "superseded", "no_effect", "unread",
            ]},
            "attempts": maybe(count.clone()),
            "runner": maybe(string.clone()),
            "outcome": maybe(string.clone()),
        },
    });
    json!({
        "$schema": crate::contract::JSON_SCHEMA_DRAFT,
        "title": SCHEMA,
        "description": "bay queue's detail: the live line, head first. Read-only; change the line with bay's verbs.",
        "type": "object",
        "required": [
            "schema", "read_at", "depth", "oldest_wait_s", "places", "arriving", "taken",
            "arrivals_listed", "unread", "from_store", "age_s", "sweep",
        ],
        "additionalProperties": false,
        "properties": {
            "schema": {"const": SCHEMA},
            "read_at": string,
            "from_store": {"type": "boolean"},
            "arrivals_listed": {"type": "boolean"},
            "age_s": maybe(count.clone()),
            "sweep": maybe(json!({
                "type": "object",
                "required": ["state"],
                "additionalProperties": false,
                "properties": {
                    "state": {"enum": ["started", "running"]},
                    "by": {"type": "object"},
                },
            })),
            "depth": count,
            "oldest_wait_s": maybe(count.clone()),
            "places": {"type": "array", "items": place},
            "arriving": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": [
                        "change", "op_id", "intent", "actor", "paths", "fixes", "after",
                        "outcome", "held_by", "requested_at", "waited_s",
                    ],
                    "additionalProperties": false,
                    "properties": {
                        "change": string,
                        "op_id": string,
                        "intent": string,
                        "actor": string,
                        "paths": strings.clone(),
                        "fixes": maybe(string.clone()),
                        "after": strings.clone(),
                        "outcome": maybe(string.clone()),
                        "held_by": strings.clone(),
                        "requested_at": maybe(string.clone()),
                        "waited_s": maybe(count.clone()),
                    },
                },
            },
            "taken": {"type": "array", "items": taken_schema()},
            "unread": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["change", "why"],
                    "additionalProperties": false,
                    "properties": {"change": string, "why": string},
                },
            },
        },
    })
}

/// G785 (a measurement, 2026-11-18: a live `bay queue` asked the remote five times: the line's
/// refs, three batches of its changes' chains and the list of merge requests, 8.9 s of 10 s on
/// the wire): the line read in two round trips. One `ls-remote` lists the line, the merge
/// requests and every change's chain head; one fetch then brings the line's refs and only the
/// chains of requested changes whose head on the remote is not this store's, settled as
/// [`Repo::remote_chains`] settles them. A chain at its remote head needs no fetch.
pub(crate) struct Line {
    /// The listing's merge requests, as `ls-remote` prints them.
    requests: String,
    /// The changes whose chains this read settled or found current: no fetch is owed for them.
    settled: BTreeSet<String>,
    /// The changes whose record here has ended: no arrival, so the arrivals fetch none of them
    /// (a retention cut leaves their old request unanswered in the record). A place is never
    /// judged by this: a place whose record here ended is fetched.
    ended: BTreeSet<String>,
}

/// The most chains the one fetch brings; a store far behind fetches the rest in batches.
const MOVED_IN_ONE_FETCH: usize = 400;

impl Line {
    /// Lists and fetches the line; `None` when the listing or the fetch failed, and the read
    /// then asks as it did before.
    fn read(repo: &Repo, unread: &mut Vec<Value>) -> Option<Self> {
        let git = &repo.git;
        let queue = crate::names::QUEUE;
        let done = crate::names::QUEUE_DONE;
        let listed = git
            .run(&[
                "ls-remote",
                repo.remote(),
                &format!("{queue}*"),
                &format!("{done}*"),
                &format!("{REQUESTS}*"),
                &format!("{CHANGES}*"),
            ])
            .ok()?;
        let mut requests = String::new();
        let mut requested: BTreeSet<String> = BTreeSet::new();
        let mut heads: BTreeMap<String, String> = BTreeMap::new();
        for line in listed.lines() {
            let Some((oid, name)) = line.split_once('\t') else {
                continue;
            };
            if let Some(change) = name.strip_prefix(REQUESTS) {
                requests.push_str(line);
                requests.push('\n');
                requested.insert(change.to_owned());
            } else if let Some(change) = name.strip_prefix(CHANGES) {
                heads.insert(change.to_owned(), oid.to_owned());
            }
        }
        let locals: BTreeMap<String, String> = git
            .run(&["for-each-ref", "--format=%(objectname) %(refname)", CHANGES])
            .ok()?
            .lines()
            .filter_map(|line| {
                let (oid, name) = line.split_once(' ')?;
                Some((name.strip_prefix(CHANGES)?.to_owned(), oid.to_owned()))
            })
            .collect();
        // A change whose record here has ended is no place and no arrival: its moved head (a
        // retention cut, say) is not this read's to settle. A store far behind would otherwise
        // settle every ended change once, one by one (a measured first read: 1,400, 153 s).
        let ended: BTreeSet<String> = Chain::new(git)
            .read_all()
            .ok()?
            .into_iter()
            .filter(|held| crate::status::ended(held.record.state))
            .map(|held| held.id)
            .collect();
        let mut current = BTreeSet::new();
        let mut moved = Vec::new();
        for change in requested.iter().filter(|change| !ended.contains(*change)) {
            match heads.get(change) {
                Some(head) if locals.get(change) == Some(head) => {
                    current.insert(change.clone());
                }
                Some(_) => moved.push(change.clone()),
                None => {}
            }
        }
        let (now, later) = moved.split_at(moved.len().min(MOVED_IN_ONE_FETCH));
        let scratch = crate::scratch::own(crate::names::FETCH);
        let specs: Vec<String> = crate::queue::refspecs()
            .into_iter()
            // G853: and the holders' leases, which name the changes a runner checks with no place.
            .chain(std::iter::once(crate::lease::refspec()))
            .chain(
                now.iter()
                    .map(|change| format!("+{}:{scratch}{change}", change_ref(change))),
            )
            .collect();
        let mut args = vec!["fetch", "-q", "--no-tags", "--prune", repo.remote()];
        args.extend(specs.iter().map(String::as_str));
        git.run(&args).ok()?;
        crate::queue::fetched_now(&repo.common_dir);
        repo.settle_fetched(now);
        unread.extend(
            repo.remote_chains(later)
                .into_iter()
                .map(|(change, why)| json!({"change": change, "why": why})),
        );
        current.extend(moved);
        Some(Self {
            requests,
            settled: current,
            ended,
        })
    }
}

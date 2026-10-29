//! `bay queue`: the live line, head first, for a person or a program (a fleet ask, 2026-10-29:
//! a tool beside bay, as lazygit is beside Git, needs the line in one fast read, never a parse
//! of `bay status`).
//!
//! It reads only what the line holds now: the reservations with no release ([`Queue::live`]),
//! and the change records of those places, fetched in one batch ([`Repo::remote_chains`]) and
//! read in one `cat-file --batch`. A change is placed by a runner, so `arriving` lists the
//! changes submitted that no runner has placed yet: with no runner running, they are the whole
//! line. No landed change and no evidence is read, so a store with thousands of places in its
//! history answers as fast as one with a few. It writes
//! nothing but fetched copies of the remote's refs; a tool changes the line only through bay's
//! own verbs (`bay merge`, `bay abandon`, `bay start <change>`).
//!
//! `detail` is one `bay.queue/1` document ([`schema`], also in `bay help --json`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{Value, json};

use crate::change::{Chain, ChangeRecord, OpState, Operation, RECORD_FILE, change_ref};
use crate::names::REQUESTS;
use crate::queue::{Place, Queue};
use crate::repo::Repo;
use crate::result::BayResult;

/// The schema name of `bay queue`'s `detail`.
pub const SCHEMA: &str = "bay.queue/1";

/// `bay queue`.
#[must_use]
pub fn queue(dir: &Path) -> BayResult {
    let repo = match Repo::open(dir) {
        Ok(repo) => repo,
        Err(error) => return BayResult::failed(error),
    };
    let mut result = read(&repo).map_or_else(BayResult::failed, BayResult::ok);
    result.freshness = repo.freshness();
    result
}

/// The `bay.queue/1` document of the line as the remote holds it now.
fn read(repo: &Repo) -> Result<Value, String> {
    let places = Queue::live(repo)?;
    let mut ids: Vec<String> = places
        .iter()
        .map(|place| place.entry.change.clone())
        .filter(|change| !change.is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    let mut unread: Vec<Value> = repo
        .remote_chains(&ids)
        .into_iter()
        .map(|(change, why)| json!({"change": change, "why": why}))
        .collect();
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
    let arriving = arriving(repo, &in_line, now, &mut unread)?;
    let oldest = places
        .iter()
        .map(|place| now.saturating_sub(place.joined))
        .chain(arriving.iter().filter_map(|row| row["waited_s"].as_u64()))
        .max();
    Ok(json!({
        "schema": SCHEMA,
        "read_at": crate::clock::rfc3339_of(now),
        "depth": rows.len(),
        "oldest_wait_s": oldest,
        "places": rows,
        "arriving": arriving,
        "unread": unread,
    }))
}

/// The changes submitted that no runner has placed yet, oldest request first. One `ls-remote`
/// lists the remote's merge requests, as a runner's arrivals do (S1). A change whose local
/// record already shows that request's operation ended or past `submitted` is passed over
/// unread; the rest are fetched in one batch and read in one.
fn arriving(
    repo: &Repo,
    placed: &BTreeSet<&str>,
    now: u64,
    unread: &mut Vec<Value>,
) -> Result<Vec<Value>, String> {
    let listed = repo
        .git
        .run(&["ls-remote", repo.remote(), &format!("{REQUESTS}*")])
        .map_err(|error| {
            format!(
                "the remote's merge requests could not be listed: {}",
                error.message()
            )
        })?;
    let held: BTreeMap<String, ChangeRecord> = Chain::new(&repo.git)
        .read_all()
        .map_err(|error| error.message())?
        .into_iter()
        .map(|held| (held.id, held.record))
        .collect();
    let requests: Vec<(String, String)> = listed
        .lines()
        .filter_map(|line| {
            let (request, name) = line.split_once('\t')?;
            let change = name.strip_prefix(REQUESTS)?;
            (!placed.contains(change)).then(|| (change.to_owned(), request.to_owned()))
        })
        .filter(|(change, request)| {
            held.get(change)
                .is_none_or(|record| waits(record, request) != Some(false))
        })
        .collect();
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let changes: Vec<String> = requests.iter().map(|(change, _)| change.clone()).collect();
    for (change, why) in repo.remote_chains(&changes) {
        unread.push(json!({"change": change, "why": why}));
    }
    let fresh = records(repo, &changes);
    let mut waiting: Vec<(&ChangeRecord, &Operation)> = Vec::new();
    for (change, request) in &requests {
        let Some(record) = fresh.iter().find(|record| &record.id == change) else {
            continue;
        };
        match waits(record, request) {
            Some(true) => waiting.extend(op_of(record, request).map(|op| (record, op))),
            Some(false) => {}
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
                "requested_at": at.map(crate::clock::rfc3339_of),
                "waited_s": at.map(|at| now.saturating_sub(at)),
            });
            (at, row)
        })
        .collect();
    rows.sort_by_key(|(at, row)| (at.unwrap_or(u64::MAX), row["change"].to_string()));
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}

/// The operation of `record` that carries merge request `request`.
fn op_of<'a>(record: &'a ChangeRecord, request: &str) -> Option<&'a Operation> {
    record
        .operations
        .iter()
        .find(|op| op.request_id.as_deref() == Some(request))
}

/// Whether `record` still waits for a runner on `request`: `None` when the record does not
/// carry that request.
fn waits(record: &ChangeRecord, request: &str) -> Option<bool> {
    let op = op_of(record, request)?;
    Some(!crate::status::ended(record.state) && op.state == OpState::Submitted)
}

/// The committer time of each of `commits`, read in one `git show`.
fn commit_times(repo: &Repo, commits: &[&str]) -> BTreeMap<String, u64> {
    if commits.is_empty() {
        return BTreeMap::new();
    }
    let mut args = vec!["show", "-s", "--format=%H %ct"];
    args.extend(commits);
    repo.git
        .run(&args)
        .map(|text| {
            text.lines()
                .filter_map(|line| {
                    let (commit, at) = line.split_once(' ')?;
                    Some((commit.to_owned(), at.trim().parse().ok()?))
                })
                .collect()
        })
        .unwrap_or_default()
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
        "joined_at": crate::clock::rfc3339_of(at.joined),
        "waited_s": now.saturating_sub(at.joined),
        "state": op.map_or_else(
            || Value::from("unread"),
            |op| serde_json::to_value(op.state).unwrap_or(Value::Null),
        ),
        "attempts": op.map(|op| op.attempts.len()),
        "runner": op.and_then(|op| op.owner.as_deref()),
        "outcome": op.and_then(|op| op.outcome.as_deref()),
    })
}

/// The JSON Schema (draft 2020-12) of `bay.queue/1`, exported by `bay help --json`.
#[must_use]
pub fn schema() -> Value {
    let string = json!({"type": "string"});
    let count = json!({"type": "integer", "minimum": 0});
    let maybe = |kind: Value| json!({"anyOf": [kind, {"type": "null"}]});
    let strings = json!({"type": "array", "items": string});
    json!({
        "$schema": crate::contract::JSON_SCHEMA_DRAFT,
        "title": SCHEMA,
        "description": "bay queue's detail: the live line, head first. Read-only; change the line with bay's verbs.",
        "type": "object",
        "required": [
            "schema", "read_at", "depth", "oldest_wait_s", "places", "arriving", "unread",
        ],
        "additionalProperties": false,
        "properties": {
            "schema": {"const": SCHEMA},
            "read_at": string,
            "depth": count,
            "oldest_wait_s": maybe(count.clone()),
            "places": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": [
                        "place", "seq", "change", "op_id", "intent", "actor", "paths", "fixes",
                        "after", "joined_at", "waited_s", "state", "attempts", "runner", "outcome",
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
                        "state": {"enum": [
                            "submitted", "admitted", "running", "checked", "published",
                            "blocked", "failed", "superseded", "no_effect", "unread",
                        ]},
                        "attempts": maybe(count.clone()),
                        "runner": maybe(string.clone()),
                        "outcome": maybe(string.clone()),
                    },
                },
            },
            "arriving": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": [
                        "change", "op_id", "intent", "actor", "paths", "fixes", "after",
                        "requested_at", "waited_s",
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
                        "requested_at": maybe(string.clone()),
                        "waited_s": maybe(count.clone()),
                    },
                },
            },
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

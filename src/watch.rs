//! `bay watch [--since <cursor>]` (`DESIGN` § 4, § 6, § 9; `INTERFACES` § 5; ruling R6): what
//! happened after the caller's cursor, so a lost notice never hides anything (G9).
//!
//! Landings are not events: they are read from trunk, every first-parent commit after the
//! cursor's trunk. Events are the `bay.event/1` records appended to the executor logs after the
//! cursor's log tips. Invalidations are derived on read through the named inputs
//! (`crate::inputs`): those of the caller's change that are stale at trunk and were not stale,
//! in the same way, at the cursor's trunk. In a change's view, events are filtered to those
//! that touch its inputs (its own, the policy's, and its named inputs') and invalidations to
//! that change; elsewhere every event is listed and no change is judged.
//!
//! The cursor is opaque to the caller: the base32 of the RFC 8785 JSON
//! `{"logs": {<executor>: <tip>}, "talks": {…}, "trunk": <commit>}`, returned as
//! `detail.cursor`. A watch from the returned cursor lists nothing new until trunk or a log
//! moves. Landings and events are each bounded per watch; a cut run sets `more`, and the
//! cursor then stops where the listing stopped. With no cursor, a change's
//! view gets its current state first: the landings since its base, its events, and every
//! invalidation it has now. Watch reads; it never writes the change record or sends a notice.
//! Like `bay status`, after a fetch it accepts and acks this actor's talks (`DESIGN` § 11c,
//! [`crate::talk::receive`]): the ack is the recipient's arrival record, not a notice.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::change::{Chain, ChangeRecord, ChangeState};
use crate::clock::now_rfc3339;
use crate::inputs::{self, Probe};
use crate::repo::Repo;
use crate::result::{BayResult, Freshness};

/// The most landings one watch lists. A longer run is listed oldest first; the returned
/// cursor stops at the last one listed, `more` is true, and the next watch goes on from there.
pub const MAX_LANDINGS: usize = 500;

/// The most events one watch reads from the logs, by the same rule: the cursor's log tips stop
/// at the last one read, and `more` is true.
pub const MAX_EVENTS: usize = 500;

/// Where a watch stopped: the trunk commit and the tip of each executor log it had read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub trunk: Option<String>,
    pub logs: BTreeMap<String, String>,
    /// Where the caller's talks were read to (ruling R6). Nothing fills it yet (C3); it is
    /// carried through, so a cursor that holds talks reads the same here.
    #[serde(default)]
    pub talks: BTreeMap<String, String>,
}

impl Cursor {
    /// Reads a cursor a watch returned.
    ///
    /// # Errors
    ///
    /// `text` is not a cursor: not base32, not the cursor's JSON, or an id that is not an
    /// object id.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = |why: &str| format!("{text:?} is not a cursor bay watch returned: {why}");
        let bytes = crate::ids::from_base32(text).ok_or_else(|| bad("not base32"))?;
        let cursor: Self =
            serde_json::from_slice(&bytes).map_err(|error| bad(&error.to_string()))?;
        let object_id = |id: &str| {
            matches!(id.len(), 40 | 64)
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        };
        if !cursor.trunk.as_deref().is_none_or(object_id)
            || !cursor.logs.values().all(|tip| object_id(tip))
        {
            return Err(bad("an id is not an object id"));
        }
        Ok(cursor)
    }

    /// The opaque text of this cursor.
    ///
    /// # Errors
    ///
    /// The cursor cannot be serialized.
    pub fn encode(&self) -> Result<String, String> {
        let value = serde_json::to_value(self).map_err(|error| error.to_string())?;
        let text = crate::canonical::canonical(&value)?;
        Ok(crate::ids::base32(text.as_bytes()))
    }
}

#[must_use]
pub fn watch(dir: &Path, since: Option<&Cursor>) -> BayResult {
    run(dir, since).unwrap_or_else(BayResult::failed)
}

fn run(dir: &Path, since: Option<&Cursor>) -> Result<BayResult, String> {
    let repo = Repo::open(dir)?;
    let change = repo.view_change(dir);
    // Best effort, as `status`: offline, the local cache is what this read can see, and
    // `freshness` says so.
    let fetched_at = repo.fetch_trunk().ok().map(|_| now_rfc3339());
    crate::log::fetch(&repo);
    // § 11c: a fetching command on the recipient's machine accepts and acks its talks.
    let talk_accept = fetched_at
        .is_some()
        .then(|| crate::talk::receive(&repo))
        .flatten();
    let trunk = repo
        .git
        .run(&["rev-parse", "--verify", &repo.trunk_tracking_ref()])
        .map_err(|error| format!("the local cache holds no trunk: {}", error.message()))?;
    // A watch the caller asked for may run a provider its cache has no answer for, as `status`
    // does (ruling R-A3-3); a hook that reads watch passes `Probe::Cached`.
    let detail = collect(&repo, change.as_deref(), since, &trunk, Probe::Run)?;
    let mut result = BayResult::ok(detail);
    if let (Some(detail), Some(accept)) = (result.detail.as_mut(), talk_accept) {
        detail["talk_accept"] = accept;
    }
    if let Some(id) = change {
        result = result.with_change(id);
    }
    result.freshness = Some(Freshness {
        trunk,
        fetched_at: fetched_at.unwrap_or_else(|| "not fetched (remote unreachable)".to_owned()),
    });
    Ok(result)
}

/// What happened after `since` up to trunk commit `trunk`, from local data only: the
/// landings, events and invalidations of the module doc, and the cursor to watch from next.
/// `change` is the caller's change, when it runs in a view.
///
/// # Errors
///
/// A git fault, or a cursor whose trunk this repository does not hold.
pub fn collect(
    repo: &Repo,
    change: Option<&str>,
    since: Option<&Cursor>,
    trunk: &str,
    probe: Probe,
) -> Result<Value, String> {
    let git = &repo.git;
    let record = match change {
        Some(id) => Some(Chain::new(git).read(id).map_err(|error| error.message())?.1),
        None => None,
    };
    let from = match (since, &record) {
        (Some(cursor), _) => cursor.trunk.clone(),
        (None, Some(record)) => Some(record.base.clone()),
        (None, None) => Some(trunk.to_owned()),
    };
    if let Some(from) = &from
        && git
            .run(&["cat-file", "-e", &format!("{from}^{{commit}}")])
            .is_err()
    {
        return Err(format!(
            "the cursor's trunk {from} is not in this repository; watch without --since for the current state"
        ));
    }
    let (landings, stopped_at) = landings(repo, from.as_deref(), trunk)?;

    let tips = crate::log::tips(git)?;
    let read_from = match (since, &record) {
        (Some(cursor), _) => cursor.logs.clone(),
        (None, Some(_)) => BTreeMap::new(),
        (None, None) => tips.clone(),
    };
    let read = crate::log::events_since(git, &read_from, &tips, MAX_EVENTS)?;
    let mut events: Vec<Value> = Vec::new();
    for event in read.events {
        let listed = record.as_ref().is_none_or(|record| touches(record, &event));
        // Two executors may write one event; its id is the same in both (`INTERFACES` § 4).
        if listed && !events.iter().any(|seen| seen["id"] == event["id"]) {
            events.push(event);
        }
    }

    // The cursor is one point: the invalidations are judged at the trunk it names.
    let reached = stopped_at.clone().unwrap_or_else(|| trunk.to_owned());
    let invalidations = match &record {
        Some(record) => invalidations(repo, record, since, &reached, probe)?,
        None => Vec::new(),
    };
    let next = Cursor {
        trunk: Some(reached),
        logs: read.tips,
        talks: since.map(|cursor| cursor.talks.clone()).unwrap_or_default(),
    };
    Ok(json!({
        "trunk": trunk,
        "since": since.map(Cursor::encode).transpose()?,
        "landings": landings,
        "events": events,
        "invalidations": inputs::to_json(invalidations.iter()),
        "more": stopped_at.is_some() || read.more,
        "cursor": next.encode()?,
    }))
}

/// The event kinds about the acceptance policy, which is an input of every change (ruling R3).
const POLICY_KINDS: [&str; 2] = ["policy_changed_outside_bay", "policy_repair"];

/// Whether `event` touches the inputs of `record`'s change (`INTERFACES` § 5): it is about that
/// change, about the policy every change is judged under, or about a named input it declared.
fn touches(record: &ChangeRecord, event: &Value) -> bool {
    if event["change"] == record.id.as_str()
        || POLICY_KINDS.iter().any(|kind| event["kind"] == *kind)
    {
        return true;
    }
    let declared = inputs::names(&record.declared_inputs);
    event["inputs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|input| input.rsplit_once('@').map(|(name, _)| name))
        .any(|name| declared.iter().any(|declared| declared.as_str() == name))
}

/// Trunk's first-parent commits after `from` (all of trunk when `None`), oldest first, at most
/// [`MAX_LANDINGS`], and the last one listed when more follow it. A landing outside bay has no
/// `change`.
fn landings(
    repo: &Repo,
    from: Option<&str>,
    trunk: &str,
) -> Result<(Vec<Value>, Option<String>), String> {
    let format = format!(
        "--format=%H%x1f%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key={},valueonly,separator=%x2c)%x1f%s%x1e",
        repo.config.actor_trailer()
    );
    let mut args = vec![
        "log".to_owned(),
        "--first-parent".to_owned(),
        "--reverse".to_owned(),
        format,
        trunk.to_owned(),
    ];
    if let Some(from) = from {
        args.push(format!("^{from}"));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let log = repo.git.run(&args).map_err(|error| error.message())?;
    let mut out = Vec::new();
    for entry in log.split('\u{1e}') {
        let fields: Vec<&str> = entry.trim().split('\u{1f}').map(str::trim).collect();
        let [commit, change, actor, subject] = fields.as_slice() else {
            continue;
        };
        if commit.is_empty() {
            continue;
        }
        if out.len() == MAX_LANDINGS {
            let last = out
                .last()
                .and_then(|landing: &Value| landing["commit"].as_str())
                .map(str::to_owned);
            return Ok((out, last));
        }
        let some = |text: &str| (!text.is_empty()).then(|| text.to_owned());
        out.push(json!({
            "commit": commit,
            "change": some(change),
            "actor": some(actor),
            "subject": subject,
        }));
    }
    Ok((out, None))
}

/// § 8: the stale inputs of a live `record` at `trunk` that were not stale in the same way
/// (state and required version) at the cursor's trunk. A landed, superseded or abandoned
/// change has none, and no provider runs for it.
fn invalidations(
    repo: &Repo,
    record: &ChangeRecord,
    since: Option<&Cursor>,
    trunk: &str,
    probe: Probe,
) -> Result<Vec<inputs::Judgement>, String> {
    let live = matches!(
        record.state,
        ChangeState::Open | ChangeState::Invalidated | ChangeState::Repairing | ChangeState::Ready
    );
    if !live
        || !crate::publish::scan(&repo.git, &record.base_first, trunk, &record.id)?
            .publications
            .is_empty()
    {
        return Ok(Vec::new());
    }
    let before = match since.map(|cursor| cursor.trunk.as_deref()) {
        Some(Some(at)) if at == trunk => return Ok(Vec::new()),
        Some(Some(at)) => inputs::stale_at(repo, record, at, probe)?,
        Some(None) | None => Vec::new(),
    };
    Ok(inputs::stale_at(repo, record, trunk, probe)?
        .into_iter()
        .filter(|now| {
            !before.iter().any(|then| {
                then.input == now.input && then.state == now.state && then.required == now.required
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::Cursor;

    #[test]
    fn a_cursor_round_trips_and_nothing_else_reads_as_one() {
        let cursor = Cursor {
            trunk: Some("a".repeat(40)),
            logs: [("exec-1".to_owned(), "b".repeat(64))].into(),
            talks: [("t1".to_owned(), "abcd".to_owned())].into(),
        };
        let text = cursor.encode().unwrap();
        assert!(
            text.bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        );
        assert_eq!(Cursor::parse(&text).unwrap(), cursor);
        let empty = Cursor::default();
        assert_eq!(Cursor::parse(&empty.encode().unwrap()).unwrap(), empty);
        let encode = |text: &str| crate::ids::base32(text.as_bytes());
        for bad in [
            "not a cursor".to_owned(),
            encode("[]"),
            encode(r#"{"trunk":"main","logs":{}}"#),
            encode(r#"{"trunk":null,"logs":{"x":"HEAD"}}"#),
            encode(r#"{"trunk":null,"logs":{},"extra":1}"#),
        ] {
            assert!(Cursor::parse(&bad).is_err(), "{bad}");
        }
        // A cursor written before talks were carried still reads.
        let old = encode(r#"{"logs":{},"trunk":null}"#);
        assert_eq!(Cursor::parse(&old).unwrap(), empty);
    }
}

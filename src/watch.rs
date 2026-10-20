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
//!
//! `bay watch --landed` (G73) is the landed stream alone, for a producer that reads every
//! landing: the same landings and cursor, no events and no invalidations. Each landing names
//! its base (the first parent) and its committer time, and one landed outside bay stays in
//! the stream with `change: null` and `outside_bay: true`. `off_first_parent` counts the
//! commits in the same range reached only through a merge's other parent: they are never
//! listed, and the count says so.
//!
//! A red trunk (G244, G244b) is derived, not logged: the cursor carries the red checks it saw,
//! and a watch lists a `trunk_red` event for each red one it did not, and a `trunk_red_cleared`
//! event for each it held that is no longer red (fixed, undone, passed or quarantined). Both
//! are judged at the cursor's trunk, and without a cursor every red check is new. In a view
//! only those whose paths cover the change's declared paths are listed; the cursor carries all.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::change::{Chain, ChangeRecord, ChangeState};
use crate::clock::now_rfc3339;
use crate::git::Git;
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
    /// The post checks trunk was red on at `trunk` (G244b). Left out when none was, so a cursor
    /// with no red reads as one written before red was carried.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub red: Vec<crate::red::Red>,
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
            || !cursor.red.iter().all(|red| object_id(&red.landing))
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
    run(dir, since, false).unwrap_or_else(BayResult::failed)
}

/// `bay watch --landed`: the landed stream of the module doc.
#[must_use]
pub fn landed(dir: &Path, since: Option<&Cursor>) -> BayResult {
    run(dir, since, true).unwrap_or_else(BayResult::failed)
}

fn run(dir: &Path, since: Option<&Cursor>, landed_only: bool) -> Result<BayResult, String> {
    let repo = match Repo::open_fetched(dir) {
        Ok(repo) => repo,
        Err(error) => return Ok(error.into_result()),
    };
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
    // G244b: a red trunk turns on records a runner elsewhere writes after the landing, so a
    // watch over a policy with post checks reads the other producers' evidence first.
    if !landed_only
        && fetched_at.is_some()
        && crate::policy::load_at(&repo.git, &trunk)
            .ok()
            .flatten()
            .is_some_and(|policy| !policy.post.is_empty())
    {
        repo.fetch_evidence(&crate::signing::Keys::load(&repo.git));
    }
    // A watch the caller asked for may run a provider its cache has no answer for, as `status`
    // does (ruling R-A3-3); a hook that reads watch passes `Probe::Cached`.
    let detail = if landed_only {
        collect_landed(&repo, change.as_deref(), since, &trunk)?
    } else {
        collect(&repo, change.as_deref(), since, &trunk, Probe::Run)?
    };
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
    let from = start(git, since, record.as_ref(), trunk)?;
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
    let registry = crate::registry::Registry::current(git).ok().flatten();
    let reds = crate::red::red(git, &reached, registry.as_ref())?;
    events.extend(red_events(
        since
            .map(|cursor| cursor.red.as_slice())
            .unwrap_or_default(),
        &reds,
        record.as_ref(),
    ));
    let next = Cursor {
        trunk: Some(reached),
        logs: read.tips,
        talks: since.map(|cursor| cursor.talks.clone()).unwrap_or_default(),
        red: reds,
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

/// `bay watch --landed` (G73): the landings of [`collect`] after `since`, the count of
/// commits off the first-parent line in the range listed, and the cursor to watch from next.
/// The cursor moves trunk only: its log tips are those a full watch from `since` would read
/// from, so a full watch from it still lists every event.
///
/// # Errors
///
/// A git fault, or a cursor whose trunk this repository does not hold.
pub fn collect_landed(
    repo: &Repo,
    change: Option<&str>,
    since: Option<&Cursor>,
    trunk: &str,
) -> Result<Value, String> {
    let git = &repo.git;
    let record = match change {
        Some(id) => Some(Chain::new(git).read(id).map_err(|error| error.message())?.1),
        None => None,
    };
    let from = start(git, since, record.as_ref(), trunk)?;
    let (landings, stopped_at) = landings(repo, from.as_deref(), trunk)?;
    let reached = stopped_at.clone().unwrap_or_else(|| trunk.to_owned());
    let logs = match (since, &record) {
        (Some(cursor), _) => cursor.logs.clone(),
        (None, Some(_)) => BTreeMap::new(),
        (None, None) => crate::log::tips(git)?,
    };
    // The red checks move with events, not trunk: a full watch from this cursor lists them.
    let next = Cursor {
        trunk: Some(reached.clone()),
        logs,
        talks: since.map(|cursor| cursor.talks.clone()).unwrap_or_default(),
        red: since.map(|cursor| cursor.red.clone()).unwrap_or_default(),
    };
    Ok(json!({
        "trunk": trunk,
        "since": since.map(Cursor::encode).transpose()?,
        "landings": landings,
        "off_first_parent": off_first_parent(git, from.as_deref(), &reached)?,
        "more": stopped_at.is_some(),
        "cursor": next.encode()?,
    }))
}

/// Where the landings start: the cursor's trunk, else the change's base in a view, else the
/// current trunk (nothing yet).
fn start(
    git: &Git,
    since: Option<&Cursor>,
    record: Option<&ChangeRecord>,
    trunk: &str,
) -> Result<Option<String>, String> {
    let from = match (since, record) {
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
    Ok(from)
}

/// The commits after `from` up to `to` (all of `to` when `from` is `None`) that are not on
/// `to`'s first-parent line: those reached only through a merge's other parent. A query over
/// the first-parent line states this count, so the commits it leaves out are never silent.
///
/// # Errors
///
/// A git fault.
pub fn off_first_parent(git: &Git, from: Option<&str>, to: &str) -> Result<u64, String> {
    let exclude = from.map(|from| format!("^{from}"));
    let count = |first_parent: bool| -> Result<u64, String> {
        let mut args = vec!["rev-list", "--count"];
        if first_parent {
            args.push("--first-parent");
        }
        args.push(to);
        args.extend(exclude.as_deref());
        let text = git.run(&args).map_err(|error| error.message())?;
        text.trim()
            .parse()
            .map_err(|_| format!("git rev-list --count printed {text:?}"))
    };
    Ok(count(false)?.saturating_sub(count(true)?))
}

/// G244b: a `trunk_red` event for each check red `now` that was not red `before`, then a
/// `trunk_red_cleared` event for each red `before` that no longer is, each in the shape of
/// [`crate::red::Red::to_json`] with the event's `schema`, `id` and `kind`. The id names the
/// check and the owning landing, so the same turn reads as the same event. In `record`'s view,
/// only those whose paths cover its declared paths.
fn red_events(
    before: &[crate::red::Red],
    now: &[crate::red::Red],
    record: Option<&ChangeRecord>,
) -> Vec<Value> {
    let event = |kind: &str, red: &crate::red::Red| {
        let mut event = red.to_json();
        event["schema"] = json!(crate::log::EVENT_SCHEMA);
        event["id"] = json!(format!(
            "{kind}:{}:{}:{}",
            red.check, red.environment, red.landing
        ));
        event["kind"] = json!(kind);
        event
    };
    let declared = record.map(|record| inputs::paths(&record.declared_inputs));
    let listed = |red: &&crate::red::Red| declared.as_ref().is_none_or(|paths| red.covers(paths));
    let turned = now
        .iter()
        .filter(|red| !before.iter().any(|then| then.same(red)))
        .filter(listed)
        .map(|red| event("trunk_red", red));
    let cleared = before
        .iter()
        .filter(|then| !now.iter().any(|red| red.same(then)))
        .filter(listed)
        .map(|then| event("trunk_red_cleared", then));
    turned.chain(cleared).collect()
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
/// [`MAX_LANDINGS`], and the last one listed when more follow it. Each names its base (the
/// first parent; `null` for a root) and its committer time. A landing outside bay has no
/// `change` and is `outside_bay`: it carries no `Change-Id`, or, with a registry, it does not
/// verify as a publication (as [`crate::publish::scan`] judges one).
fn landings(
    repo: &Repo,
    from: Option<&str>,
    trunk: &str,
) -> Result<(Vec<Value>, Option<String>), String> {
    let format = format!(
        "--format=%H%x1f%P%x1f%ct%x1f%cn%x1f%(trailers:key=Change-Id,valueonly,separator=%x2c)%x1f%(trailers:key=Registry-Id,valueonly,separator=%x2c)%x1f%(trailers:key={},valueonly,separator=%x2c)%x1f%s%x1e",
        repo.config.actor_trailer()
    );
    let signatures = repo
        .git
        .ref_target(crate::registry::REGISTRY_REF)
        .map_err(|error| error.message())?
        .is_some();
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
        let [
            commit,
            parents,
            time,
            committer,
            change,
            registry_id,
            actor,
            subject,
        ] = fields.as_slice()
        else {
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
        let outside_bay = change.is_empty()
            || (signatures
                && !crate::publish::verify_publication(&repo.git, commit, registry_id, committer));
        out.push(json!({
            "commit": commit,
            "change": (!outside_bay).then(|| (*change).to_owned()),
            "outside_bay": outside_bay,
            "base": parents.split_whitespace().next(),
            "actor": some(actor),
            "at": time.parse().ok().map(crate::clock::rfc3339_of),
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
            red: vec![crate::red::Red {
                check: "suite".to_owned(),
                environment: "local".to_owned(),
                paths: Some(vec!["src/**".to_owned()]),
                landing: "c".repeat(40),
                change: None,
            }],
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
            encode(
                r#"{"trunk":null,"logs":{},"red":[{"check":"s","environment":"e","paths":null,"landing":"HEAD","change":null}]}"#,
            ),
        ] {
            assert!(Cursor::parse(&bad).is_err(), "{bad}");
        }
        // A cursor written before talks were carried still reads.
        let old = encode(r#"{"logs":{},"trunk":null}"#);
        assert_eq!(Cursor::parse(&old).unwrap(), empty);
    }
}

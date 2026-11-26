//! BACKLOG 326: an expedite. An agent asks for one change to run next, right behind the running
//! head (`bay merge --expedite <why>`). The ask rides in the change's agent-signed request; a
//! runner grants it as a queue place ranked right behind that head (`crate::queue::Grant`), within
//! the repository's limits: `[expedite] per_day` grants a UTC day, whoever asks, and one live at a
//! time. A granted place runs alone and ends a batch before it, and a change that could land from
//! its own place past the head (G552, G703) waits behind it. A repair of a red main goes first
//! without one (G585). An ask that is not granted leaves the change in line as a plain arrival.

use serde_json::{Value, json};

use crate::canonical::digest;
use crate::git::Git;
use crate::queue::Queue;

/// The event kind (`bay.event/1`) of an expedite decision: one per operation that asked.
pub const KIND: &str = "expedite";

/// How many expedites may be live at once (the ruling's cap): a second would be the line again.
pub const MAX_LIVE: usize = 1;

/// Why an ask was not granted (`detail.expedite.refused`, `EXPEDITE_REFUSED`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The repository grants none: no `[expedite]` in its config on main.
    Off,
    /// [`MAX_LIVE`] expedites are live already.
    Live,
    /// The UTC day's `[expedite] per_day` grants are spent.
    Spent,
}

impl Refused {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Live => "live",
            Self::Spent => "spent",
        }
    }
}

/// The reason an operation's request gives for its expedite (`expedite.why` in its
/// `request.json`), or `None` when it asks none.
#[must_use]
pub fn asked(git: &Git, request_id: &str) -> Option<String> {
    let text = git.file_at(request_id, "request.json").ok()??;
    let request: Value = serde_json::from_str(&text).ok()?;
    request["expedite"]["why"].as_str().map(str::to_owned)
}

/// Whether `queue` may grant one more expedite at `now` (Unix seconds) under `per_day`
/// (`[expedite] per_day`): `None` when it may.
#[must_use]
pub fn refusal(per_day: Option<u32>, queue: &Queue, now: u64) -> Option<Refused> {
    let Some(per_day) = per_day else {
        return Some(Refused::Off);
    };
    if queue.expedites().len() >= MAX_LIVE {
        return Some(Refused::Live);
    }
    if queue.expedited_on_day(now) >= usize::try_from(per_day).unwrap_or(usize::MAX) {
        return Some(Refused::Spent);
    }
    None
}

/// `detail.expedite` of a result: the ask, whether it was refused and why, and the limits as
/// `queue` holds them at `now`.
#[must_use]
pub fn detail(
    why: &str,
    refused: Option<Refused>,
    per_day: Option<u32>,
    queue: &Queue,
    now: u64,
) -> Value {
    json!({
        "why": why,
        "refused": refused.map(Refused::code),
        "live": queue.expedites().len(),
        "max_live": MAX_LIVE,
        "spent_today": queue.expedited_on_day(now),
        "per_day": per_day,
        "resets_at": crate::clock::rfc3339_of((now / 86_400 + 1) * 86_400),
    })
}

/// What a granted place's entry names besides its operation (`expedite` in `entry.json`): the
/// reason, who asked, the request that asked, and the runner that granted it.
#[must_use]
pub fn grant_entry(why: &str, actor: &str, request_id: &str, by: &str) -> Value {
    json!({"why": why, "actor": actor, "request": request_id, "by": by})
}

/// One decision on an ask.
pub struct Decided<'a> {
    pub change: &'a str,
    pub op_id: &'a str,
    pub actor: &'a str,
    pub why: &'a str,
    pub by: &'a str,
    /// The refusal, or `None` for a grant.
    pub refused: Option<Refused>,
    /// The granted place, and the head's place it stands behind.
    pub place: Option<u64>,
    pub after: Option<u64>,
}

/// The `expedite` event of `decided`: its id is the operation's, so one ask is one event.
///
/// # Errors
///
/// The id cannot be digested.
pub fn event(decided: &Decided<'_>) -> Result<Value, String> {
    let id = digest(&json!({"kind": KIND, "op_id": decided.op_id}))?;
    Ok(json!({
        "schema": crate::log::EVENT_SCHEMA,
        "id": format!("evt_{}", &id.trim_start_matches("sha256:")[..20]),
        "at": crate::clock::now_rfc3339(),
        "kind": KIND,
        "change": decided.change,
        "op_id": decided.op_id,
        "actor": {"name": decided.actor},
        "why": decided.why,
        "by": decided.by,
        "granted": decided.refused.is_none(),
        "refused": decided.refused.map(Refused::code),
        "place": decided.place,
        "after": decided.after,
        "recurrence_key": format!("{KIND}/{}", decided.actor),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{EXPEDITE_OVER, Entry};

    const DAY: u64 = 20_000 * 86_400;

    fn place(seq: u64, expedite: bool, joined: u64) -> Entry {
        Entry {
            seq,
            commit: String::new(),
            op_id: format!("c{seq}.1"),
            change: format!("c{seq}"),
            rank: seq,
            expedite,
            joined,
        }
    }

    #[test]
    fn an_ask_is_refused_while_off_live_or_spent_and_the_budget_resets_each_utc_day() {
        let mut queue = Queue::default();
        assert_eq!(refusal(None, &queue, DAY), Some(Refused::Off));
        assert_eq!(refusal(Some(1), &queue, DAY), None);
        queue.entries.insert(1, place(1, false, DAY));
        queue.entries.insert(2, place(2, true, DAY + 10));
        assert_eq!(refusal(Some(5), &queue, DAY + 20), Some(Refused::Live));
        queue.released.insert(2);
        assert_eq!(refusal(Some(1), &queue, DAY + 20), Some(Refused::Spent));
        assert_eq!(refusal(Some(2), &queue, DAY + 20), None);
        assert_eq!(
            refusal(Some(1), &queue, DAY + 86_400),
            None,
            "a new UTC day"
        );
        // A grant that lost a race to another one was never granted.
        queue.release_reasons.insert(2, EXPEDITE_OVER.to_owned());
        assert_eq!(refusal(Some(1), &queue, DAY + 20), None);
        let detail = detail("why", None, Some(1), &queue, DAY + 20);
        assert_eq!(detail["spent_today"], 0, "{detail}");
        assert_eq!(detail["resets_at"], crate::clock::rfc3339_of(DAY + 86_400));
    }

    #[test]
    fn of_two_grants_pushed_at_once_the_later_is_over() {
        let mut queue = Queue::default();
        queue.entries.insert(1, place(1, false, DAY));
        queue.entries.insert(2, place(2, true, DAY + 10));
        queue.entries.insert(3, place(3, true, DAY + 11));
        assert_eq!(queue.over(2, 5, DAY + 20), None);
        assert_eq!(queue.over(3, 5, DAY + 20), Some(Refused::Live));
        queue.released.insert(2);
        assert_eq!(queue.over(3, 1, DAY + 20), Some(Refused::Spent));
        assert_eq!(queue.over(3, 2, DAY + 20), None);
        assert!(
            queue.expedite_ahead(4),
            "a place behind the expedite waits for it"
        );
        assert!(!queue.expedite_ahead(3));
    }
}

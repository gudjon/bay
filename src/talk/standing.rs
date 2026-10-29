//! What `bay status` shows of each open talk (G47, `DESIGN` § 11c "What status shows"): who owes
//! the reply, the age from `sent`, the ack's age and the reply's age, and the talk's standing.
//!
//! - `not_on_recipient`: no ack yet, within [`BAY_TALK_ACK_BOUND_S`] of `sent`.
//! - `lost`: no ack by that bound: a lost arrival. The row stays.
//! - `arrived`: acked, no backbrief yet, before `follow_up`.
//! - `reply_owed`: a backbrief came, and the talk is still open, before `follow_up`.
//! - `stale`: acked, and the `follow_up` it owes (the latest backbrief's, or the body's) has
//!   passed. The row stays.
//! - `expired` (G52, ruling T3): read after its `expires`, judged by the ack's commit time, or not
//!   read yet with `expires` passed, so any read is later. It owes nothing: `owes` is null, and
//!   it is never `lost` or `stale`. The recipient's accept closes a talk read after `expires`.
//!
//! A closed talk is not listed. A backbrief that says `done` closes nothing: only evidence does.
//!
//! Each row says `signed`: whether a key signed the talk's sender. Under `[policy] signing =
//! "approvals"` (G445) it is `false`, and `from` is the name the sender claims.

use serde_json::{Value, json};

use super::{Entry, TALK_PREFIX, Talk, read_after_expiry, talks_where};
use crate::clock::unix_of;
use crate::git::Git;

const TEST_HOOKS: bool = option_env!("BAY_RELEASE_VERSION").is_none();

/// No ack within this many seconds of `sent` is a lost arrival (`DESIGN` § 11c: 24 hours).
pub const BAY_TALK_ACK_BOUND_S: u64 = 24 * 3600;

/// The standing of one open talk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    NotOnRecipient,
    Lost,
    Arrived,
    ReplyOwed,
    Stale,
    Expired,
}

impl Standing {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotOnRecipient => "not_on_recipient",
            Self::Lost => "lost",
            Self::Arrived => "arrived",
            Self::ReplyOwed => "reply_owed",
            Self::Stale => "stale",
            Self::Expired => "expired",
        }
    }
}

/// The reader's clock for talk standings: this machine's, or in a test build shifted by
/// `BAY_TEST_TALK_CLOCK_SKEW_S` seconds, so G52 reads with a skewed clock.
#[must_use]
pub fn reader_now() -> u64 {
    let skew = std::env::var("BAY_TEST_TALK_CLOCK_SKEW_S")
        .ok()
        .filter(|_| TEST_HOOKS)
        .and_then(|text| text.parse::<i64>().ok())
        .unwrap_or(0);
    crate::live::unix_now().saturating_add_signed(skew)
}

/// The standing of an open talk sent at `sent`, acked at `acked` (the ack's commit time), last
/// backbriefed at `replied`, owing a reply by `follow_up`, void after `expires`, looked at `now`
/// (all Unix seconds).
#[must_use]
pub fn standing(
    sent: u64,
    acked: Option<u64>,
    replied: Option<u64>,
    follow_up: Option<u64>,
    expires: Option<u64>,
    now: u64,
) -> Standing {
    match acked {
        // Not read yet and void now: any read is after `expires`, so nothing is owed.
        None if read_after_expiry(now, expires) => Standing::Expired,
        None if now >= sent.saturating_add(BAY_TALK_ACK_BOUND_S) => Standing::Lost,
        None => Standing::NotOnRecipient,
        // G52: read after `expires`, judged by the ack's commit time, never by `now`.
        Some(read) if read_after_expiry(read, expires) => Standing::Expired,
        Some(_) if follow_up.is_some_and(|due| now > due) => Standing::Stale,
        Some(_) if replied.is_some() => Standing::ReplyOwed,
        Some(_) => Standing::Arrived,
    }
}

/// The open talks `me` sent or must answer, oldest first, as `bay status` lists them.
///
/// # Errors
///
/// A ref that is not a talk, or a git fault.
pub fn rows(git: &Git, me: &str, now: u64) -> Result<Vec<Value>, String> {
    open_where(git, now, |talk| talk.from_name() == me || talk.who == me)
}

/// Every open talk this cache holds, oldest first, as [`rows`] lists them: the fleet map's talks
/// (`DESIGN` § 11a, G47b). The cache holds the talks to and from the actors that fetch here.
///
/// # Errors
///
/// A ref that is not a talk, or a git fault.
pub fn open_rows(git: &Git, now: u64) -> Result<Vec<Value>, String> {
    open_where(git, now, |_| true)
}

fn open_where(git: &Git, now: u64, keep: impl Fn(&Talk) -> bool) -> Result<Vec<Value>, String> {
    let mut talks = talks_where(git, TALK_PREFIX, |talk| !talk.closed() && keep(talk))?;
    talks.sort_by_key(Talk::sent);
    talks.iter().map(|talk| row(git, talk, now)).collect()
}

/// One talk as `bay status --since` follows it (G47b).
#[derive(Clone, Debug)]
pub struct Followed {
    /// The talk's row, as [`rows`] lists it.
    pub row: Value,
    /// How it closed (`answered`, `superseded` or `expired`); `None` while it is open. A
    /// discovery talk closed by its answer, with no close entry, closed as `answered`.
    pub closed: Option<String>,
    /// How many replies it holds: its backbriefs and answers.
    pub replies: usize,
}

/// The talks `me` sent or must answer, oldest first, as `bay status --since` follows them: every open one,
/// and each closed one `held` keeps (those its cursor held open), so a close is seen once.
///
/// # Errors
///
/// A ref that is not a talk, or a git fault.
pub fn followed(
    git: &Git,
    me: &str,
    now: u64,
    held: impl Fn(&str) -> bool,
) -> Result<Vec<Followed>, String> {
    let mut talks = talks_where(git, TALK_PREFIX, |talk| {
        (talk.from_name() == me || talk.who == me) && (!talk.closed() || held(&talk.id))
    })?;
    talks.sort_by_key(Talk::sent);
    talks
        .iter()
        .map(|talk| -> Result<Followed, String> {
            let closed = if talk.closed() {
                Some(
                    talk.closed_as(git)?
                        .unwrap_or_else(|| "answered".to_owned()),
                )
            } else {
                None
            };
            let replies = talk
                .entries
                .iter()
                .filter(|entry| matches!(entry.kind.as_str(), "backbrief" | "answer"))
                .count();
            Ok(Followed {
                row: row(git, talk, now)?,
                closed,
                replies,
            })
        })
        .collect()
}

/// One talk's row, as `bay status` lists it, and the round table reads it (T2).
pub(crate) fn row(git: &Git, talk: &Talk, now: u64) -> Result<Value, String> {
    let entry = |entry: &Entry| -> Result<Value, String> {
        let text = git
            .run(&["cat-file", "blob", &entry.blob])
            .map_err(|error| error.message())?;
        serde_json::from_str(&text).map_err(|error| error.to_string())
    };
    let at = |value: &Value| value["at"].as_str().and_then(unix_of);
    let ack = match talk.entries.iter().find(|entry| entry.kind == "ack") {
        Some(found) => Some(entry(found)?),
        None => None,
    };
    let acked = ack.as_ref().and_then(at);
    // G53: the wake adapter's answer, as the recipient's ack recorded it.
    let wake = ack.as_ref().map_or(Value::Null, |ack| ack["wake"].clone());
    let backbrief = match talk
        .entries
        .iter()
        .rev()
        .find(|entry| entry.kind == "backbrief")
    {
        Some(found) => Some(entry(found)?),
        None => None,
    };
    let replied = backbrief.as_ref().and_then(at);
    let follow_up = talk.follow_up(git)?;
    let due = follow_up.as_deref().and_then(unix_of);
    let sent = talk.sent();
    let held = standing(sent, acked, replied, due, talk.expires(), now);
    let owes = if held == Standing::Expired {
        Value::Null
    } else {
        json!(
            backbrief
                .as_ref()
                .and_then(|value| value["owes"].as_str().map(str::to_owned))
                .unwrap_or_else(|| talk.who.clone())
        )
    };
    let age = |at: u64| now.saturating_sub(at);
    Ok(json!({
        "talk": talk.id,
        "kind": talk.body["kind"],
        "from": talk.from_name(),
        // G445: whether a key signed the sender, or `from` is a name it claims (`approvals`).
        "signed": talk.signed(git)?,
        "who": talk.who,
        "owes": owes,
        "what": talk.body["what"],
        "change": talk.about_change(),
        "sent": talk.body["sent"],
        "age_s": age(sent),
        "ack_age_s": acked.map(age),
        "reply_age_s": replied.map(age),
        "follow_up": follow_up,
        "wake": wake,
        "expires": talk.body["expires"],
        "standing": held.as_str(),
    }))
}

#[cfg(test)]
mod tests {
    use super::{BAY_TALK_ACK_BOUND_S, Standing, standing};

    /// G47: each standing, and the bounds between them.
    #[test]
    fn a_talk_with_no_ack_or_no_backbrief_stays_visible() {
        let sent = 1_000;
        let bound = sent + BAY_TALK_ACK_BOUND_S;
        assert_eq!(
            standing(sent, None, None, None, None, sent + 5),
            Standing::NotOnRecipient
        );
        assert_eq!(
            standing(sent, None, None, None, None, bound - 1),
            Standing::NotOnRecipient
        );
        assert_eq!(
            standing(sent, None, None, None, None, bound),
            Standing::Lost
        );
        let due = sent + 600;
        assert_eq!(
            standing(sent, Some(sent + 9), None, Some(due), None, sent + 60),
            Standing::Arrived
        );
        assert_eq!(
            standing(
                sent,
                Some(sent + 9),
                Some(sent + 30),
                Some(due),
                None,
                sent + 60
            ),
            Standing::ReplyOwed
        );
        assert_eq!(
            standing(sent, Some(sent + 9), None, Some(due), None, due + 1),
            Standing::Stale
        );
        assert_eq!(
            standing(
                sent,
                Some(sent + 9),
                Some(sent + 30),
                Some(due),
                None,
                due + 1
            ),
            Standing::Stale,
            "a backbrief that keeps the old follow_up is stale after it"
        );
    }

    /// G52: an expired talk owes nothing, and the read is judged by the ack's time, never by the
    /// clock it is looked at with.
    #[test]
    fn an_expired_talk_owes_nothing() {
        let sent = 1_000;
        let (expires, due) = (sent + 300, sent + 600);
        let late = sent + BAY_TALK_ACK_BOUND_S + 1;
        let read_in_time = Some(sent + 9);
        assert_eq!(
            standing(
                sent,
                read_in_time,
                None,
                Some(due),
                Some(expires),
                expires + 1
            ),
            Standing::Arrived,
            "read before `expires`: the backbrief is owed after it"
        );
        assert_eq!(
            standing(sent, read_in_time, None, Some(due), Some(expires), late),
            Standing::Stale
        );
        assert_eq!(
            standing(
                sent,
                Some(expires + 1),
                None,
                Some(due),
                Some(expires),
                expires + 2
            ),
            Standing::Expired
        );
        assert_eq!(
            standing(
                sent,
                Some(expires + 1),
                None,
                Some(due),
                Some(expires),
                late
            ),
            Standing::Expired,
            "never stale"
        );
        assert_eq!(
            standing(sent, None, None, Some(due), Some(expires), late),
            Standing::Expired,
            "never lost"
        );
        assert_eq!(
            standing(sent, None, None, Some(due), Some(expires), expires),
            Standing::NotOnRecipient
        );
    }
}

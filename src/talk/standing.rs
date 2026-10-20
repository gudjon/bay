//! What `bay status` shows of each open talk (G47, `DESIGN` § 11c "What status shows"): who owes
//! the reply, the age from `sent`, the ack's age and the reply's age, and the talk's standing.
//!
//! - `not_on_recipient`: no ack yet, within [`BAY_TALK_ACK_BOUND_S`] of `sent`.
//! - `lost`: no ack by that bound: a lost arrival. The row stays.
//! - `arrived`: acked, no backbrief yet, before `follow_up`.
//! - `reply_owed`: a backbrief came, and the talk is still open, before `follow_up`.
//! - `stale`: acked, and the `follow_up` it owes (the latest backbrief's, or the body's) has
//!   passed. The row stays.
//!
//! A closed talk is not listed. A backbrief that says `done` closes nothing: only evidence does.

use serde_json::{Value, json};

use super::{Entry, TALK_PREFIX, Talk, talks_where};
use crate::clock::unix_of;
use crate::git::Git;

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
        }
    }
}

/// The standing of an open talk sent at `sent`, acked at `acked`, last backbriefed at
/// `replied`, owing a reply by `follow_up`, read at `now` (all Unix seconds).
#[must_use]
pub fn standing(
    sent: u64,
    acked: Option<u64>,
    replied: Option<u64>,
    follow_up: Option<u64>,
    now: u64,
) -> Standing {
    match acked {
        None if now >= sent.saturating_add(BAY_TALK_ACK_BOUND_S) => Standing::Lost,
        None => Standing::NotOnRecipient,
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
    let mut talks = talks_where(git, TALK_PREFIX, |talk| {
        !talk.closed() && (talk.from_name() == me || talk.who == me)
    })?;
    talks.sort_by_key(Talk::sent);
    talks.iter().map(|talk| row(git, talk, now)).collect()
}

fn row(git: &Git, talk: &Talk, now: u64) -> Result<Value, String> {
    let entry = |entry: &Entry| -> Result<Value, String> {
        let text = git
            .run(&["cat-file", "blob", &entry.blob])
            .map_err(|error| error.message())?;
        serde_json::from_str(&text).map_err(|error| error.to_string())
    };
    let at = |value: &Value| value["at"].as_str().and_then(unix_of);
    let acked = match talk.entries.iter().find(|entry| entry.kind == "ack") {
        Some(found) => at(&entry(found)?),
        None => None,
    };
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
    let owes = backbrief
        .as_ref()
        .and_then(|value| value["owes"].as_str().map(str::to_owned))
        .unwrap_or_else(|| talk.who.clone());
    let age = |at: u64| now.saturating_sub(at);
    Ok(json!({
        "talk": talk.id,
        "kind": talk.body["kind"],
        "from": talk.from_name(),
        "who": talk.who,
        "owes": owes,
        "what": talk.body["what"],
        "sent": talk.body["sent"],
        "age_s": age(sent),
        "ack_age_s": acked.map(age),
        "reply_age_s": replied.map(age),
        "follow_up": follow_up,
        "standing": standing(sent, acked, replied, due, now).as_str(),
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
            standing(sent, None, None, None, sent + 5),
            Standing::NotOnRecipient
        );
        assert_eq!(
            standing(sent, None, None, None, bound - 1),
            Standing::NotOnRecipient
        );
        assert_eq!(standing(sent, None, None, None, bound), Standing::Lost);
        let due = sent + 600;
        assert_eq!(
            standing(sent, Some(sent + 9), None, Some(due), sent + 60),
            Standing::Arrived
        );
        assert_eq!(
            standing(sent, Some(sent + 9), Some(sent + 30), Some(due), sent + 60),
            Standing::ReplyOwed
        );
        assert_eq!(
            standing(sent, Some(sent + 9), None, Some(due), due + 1),
            Standing::Stale
        );
        assert_eq!(
            standing(sent, Some(sent + 9), Some(sent + 30), Some(due), due + 1),
            Standing::Stale,
            "a backbrief that keeps the old follow_up is stale after it"
        );
    }
}

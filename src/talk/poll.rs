//! How often a session fetches its talks (G54; ruling T5): a live session at most once per
//! [`BAY_TALK_POLL_S`], and an idle wait every [`BAY_TALK_POLL_S`], doubling after each
//! [`BACKOFF_AFTER`] empty polls up to [`MAX_POLL_S`], ending (exit 2, the wake) at the first talk
//! it accepts. No resident daemon: the wait is a command an asynchronous hook runs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::repo::Repo;

/// The poll interval of a live session and the first interval of an idle wait.
pub const BAY_TALK_POLL_S: u64 = 30;
/// Empty polls before an idle wait doubles its interval.
pub const BACKOFF_AFTER: u32 = 10;
/// The longest interval of an idle wait.
pub const MAX_POLL_S: u64 = 300;

const TEST_HOOKS: bool = option_env!("BAY_RELEASE_VERSION").is_none();

/// The interval after `empty` empty polls, in seconds.
#[must_use]
pub fn interval(empty: u32) -> u64 {
    (0..empty / BACKOFF_AFTER).fold(BAY_TALK_POLL_S, |at, _| (at * 2).min(MAX_POLL_S))
}

/// One second of the schedule, as the clock sleeps it: a second, or in a test build
/// `BAY_TEST_TALK_POLL_MS` milliseconds, so G54 runs the schedule fast.
fn tick() -> Duration {
    let millis = std::env::var("BAY_TEST_TALK_POLL_MS")
        .ok()
        .filter(|_| TEST_HOOKS)
        .and_then(|text| text.parse().ok())
        .unwrap_or(1000);
    Duration::from_millis(millis)
}

fn state_file(repo: &Repo) -> PathBuf {
    repo.common_dir.join("bay").join("talk-poll.json")
}

fn read_last(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&text).ok()?["last"].as_u64()
}

fn write_last(path: &Path, last: u64) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, json!({"last": last}).to_string()).is_ok() {
        let _ = crate::durable::rename(&tmp, path);
    }
}

/// F1: whether this clone may fetch the key registry for its talks now, at most once per
/// [`BAY_TALK_POLL_S`], so a clone with no registry, or one that fetched it before a sender was
/// enrolled, pays one fetch per poll and not one per command. Records the fetch when it is due.
#[must_use]
pub fn registry_due(repo: &Repo, now: u64) -> bool {
    let path = repo.common_dir.join("bay").join("talk-registry.json");
    if read_last(&path).is_some_and(|last| now < last.saturating_add(BAY_TALK_POLL_S)) {
        return false;
    }
    write_last(&path, now);
    true
}

/// A live session's hook (`UserPromptSubmit`, `PostToolUse`): accepts this actor's talks when
/// the last fetch was [`BAY_TALK_POLL_S`] or more ago, and returns what it accepted; `None`
/// when it was not due.
#[must_use]
pub fn live(repo: &Repo, now: u64) -> Option<Value> {
    let path = state_file(repo);
    if read_last(&path).is_some_and(|last| now < last.saturating_add(BAY_TALK_POLL_S)) {
        return None;
    }
    write_last(&path, now);
    crate::talk::receive(repo)
}

/// A fetching command (`bay status`, `bay status --since`): accepts this actor's talks as a live
/// session's hook does, at most once per [`BAY_TALK_POLL_S`] and on the same state, so a status
/// right after a prompt fetches nothing again (G445). Inside the interval the outcome is
/// `not_due`, naming the last fetch; `None` where no talk can exist here.
#[must_use]
pub fn command(repo: &Repo, now: u64) -> Option<Value> {
    if TEST_HOOKS && std::env::var_os("BAY_TEST_TALK_COMMAND_UNBOUNDED").is_some() {
        return crate::talk::receive(repo);
    }
    let path = state_file(repo);
    if let Some(last) = read_last(&path).filter(|last| now < last.saturating_add(BAY_TALK_POLL_S)) {
        return super::accept::not_due(repo, last, now);
    }
    write_last(&path, now);
    crate::talk::receive(repo)
}

/// `bay talk wait`: polls this actor's talks on the schedule of [`interval`] for at most
/// `max_s` seconds. Returns 2 (the wake) with the accepted talks when one arrives, else 0.
#[must_use]
pub fn wait(repo: &Repo, max_s: u64) -> (u8, Value) {
    let mut empty = 0u32;
    let mut slept = 0u64;
    loop {
        if let Some(outcome) = crate::talk::receive(repo)
            && outcome["acked"]
                .as_array()
                .is_some_and(|acked| !acked.is_empty())
        {
            return (
                2,
                json!({"woken": true, "polls": empty + 1, "accepted": outcome}),
            );
        }
        empty += 1;
        let step = interval(empty);
        if slept + step > max_s {
            return (0, json!({"woken": false, "polls": empty}));
        }
        std::thread::sleep(tick() * u32::try_from(step).unwrap_or(u32::MAX));
        slept += step;
    }
}

#[cfg(test)]
mod tests {
    use super::{BACKOFF_AFTER, BAY_TALK_POLL_S, MAX_POLL_S, interval};

    /// G54: ten empty polls double the interval, up to the bound.
    #[test]
    fn ten_empty_polls_double_the_interval_up_to_the_bound() {
        assert_eq!(interval(0), BAY_TALK_POLL_S);
        assert_eq!(interval(BACKOFF_AFTER - 1), BAY_TALK_POLL_S);
        assert_eq!(interval(BACKOFF_AFTER), BAY_TALK_POLL_S * 2);
        assert_eq!(interval(BACKOFF_AFTER * 2), BAY_TALK_POLL_S * 4);
        assert_eq!(interval(BACKOFF_AFTER * 10), MAX_POLL_S);
    }
}

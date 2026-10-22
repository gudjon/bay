//! Flow (G309) and the review queue (G310), derived on read from the change chains this
//! repository holds, never stored.
//!
//! `bay status` names the flow of work under `detail.flow`: how many changes are live, by
//! actor, beside the WIP limit the policy sets (`[agent] max_live_changes`, G264); how many
//! landed in the last 7 days; the cycle time, start to landing, of those landed in the last 30;
//! the oldest live change; and how many live changes wait for a person's approval. It reads the
//! chains and landed markers as the WIP limit reads them ([`crate::overlap::read_chains`],
//! [`crate::overlap::counts_live`]), so the two never disagree on what is live, and makes at most
//! two more git calls: one `cat-file --batch` of the commits whose times it needs and one
//! first-parent walk of the chains it times. No fetch beyond what `bay status` already makes.
//!
//! G316: the flow also names the queue wait and the check time of those landings, p50 and p90.
//! It does not time them again: each operation's terminal record already holds its queue wait
//! and its check runs ([`crate::timing`]), so the flow reads the published records of the
//! changes landed in the last 30 days from the executor logs, in three more git calls however
//! long the logs are ([`crate::log::published_of`]), and none when nothing landed then.
//!
//! A record keeps no clock of its own, so each time is the committer time of the commit that
//! recorded the event: a change starts at the earliest commit of its chain's first-parent line
//! above its first base (its first draft, or its first state commit); it lands at the
//! publication its landed marker names, or, without a marker here, at its chain head once the
//! chain says landed; and it waits for an approval since the state commit that carries its land
//! request (`Request-Id`).
//!
//! G310: under `[limits] max_waiting_approvals = N`, [`review_queue`] is what `bay land` counts
//! before it submits a change whose paths need an approval check.

use std::collections::{BTreeSet, HashMap};

use serde_json::{Value, json};

use crate::change::{ChangeState, HeldChain};
use crate::git::Git;
use crate::repo::Repo;

/// One day, in seconds.
const DAY_S: u64 = 86_400;
/// The window `landed` counts over.
const LANDED_DAYS: u64 = 7;
/// The window `cycle_time_s` is judged over.
const CYCLE_DAYS: u64 = 30;

/// The live changes that wait for a person's approval, and the oldest of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Review {
    /// How many live changes wait for an approval (`APPROVAL_PENDING`).
    pub waiting: u64,
    /// The one that has waited longest, by the time of its land request.
    pub oldest: Option<String>,
    /// How long that one has waited, in seconds.
    pub age_s: Option<u64>,
}

/// G310: the live changes of this repository, other than `except`, whose last operation waits
/// for a person's approval ([`crate::publish::awaits_approval`]), by the chains this repository
/// holds, as the WIP limit counts live changes. No fetch.
///
/// # Errors
///
/// The chains, the landed markers or the request commits cannot be read.
pub fn review_queue(repo: &Repo, except: Option<&str>) -> Result<Review, String> {
    let (chains, landed) = crate::overlap::read_chains(repo)?;
    let waiting: Vec<&HeldChain> = chains
        .iter()
        .filter(|held| Some(held.id.as_str()) != except)
        .filter(|held| crate::overlap::counts_live(held, &landed) && awaits_approval(held))
        .collect();
    let requests: Vec<String> = waiting.iter().filter_map(|held| request_of(held)).collect();
    let times = commit_times(&repo.git, &requests)?;
    Ok(review_of(&waiting, &times, crate::live::unix_now()))
}

/// G309: `detail.flow` of `bay status`, judged at `trunk` (whose policy sets the WIP limit):
///
/// - `live`: how many changes are live (not landed, abandoned or superseded; a lane change
///   counts), as the WIP limit counts them;
/// - `by_actor`: for each actor holding a live change, in name order, `{actor, live, limit}`,
///   `limit` the policy's `[agent] max_live_changes` (null when it sets none);
/// - `landed`: how many changes landed in the last 7 days;
/// - `cycle_time_s`: `{p50, p90, landed}`, the median and 90th percentile in seconds from start
///   to landing of the changes landed in the last 30 days, and how many those are; null when
///   none landed then;
/// - `queue_wait_s` (G316): `{p50, p90, landed}`, the median and 90th percentile in seconds of
///   the queue wait of the changes landed in the last 30 days, and how many carry one; null when
///   none does. Each is the `queue_wait_s` of the change's published terminal record
///   ([`crate::timing`]): from the state commit that gave it a queue place (`queued <op>`), or
///   from its land request when it never queued, to its first claim after that, when it became
///   the head an executor works on. A landing whose record carries none is left out;
/// - `check_time_s` (G316): `{p50, p90, runs}`, the median and 90th percentile in seconds of the
///   check runs of those same landings, and how many runs; null when none is timed. A run is one
///   attempt row (`checks[]`) of the published terminal record: from its candidate commit to the
///   chain state that first records it checked or blocked;
/// - `oldest_live`: `{change, age_s}`, the live change that started first and its age in
///   seconds; null when none is live;
/// - `waiting_approval`: `{count, oldest, age_s}`, how many live changes wait for a person's
///   approval, the one that has waited longest and for how long since its land request.
///
/// # Errors
///
/// A git fault reading the chains, the markers, the policy or the commit times.
pub fn flow(repo: &Repo, trunk: &str) -> Result<Value, String> {
    let now = crate::live::unix_now();
    let (chains, landed) = crate::overlap::read_chains(repo)?;
    let live: Vec<&HeldChain> = chains
        .iter()
        .filter(|held| crate::overlap::counts_live(held, &landed))
        .collect();
    let waiting: Vec<&HeldChain> = live
        .iter()
        .copied()
        .filter(|held| awaits_approval(held))
        .collect();
    let mut wanted: Vec<String> = chains
        .iter()
        .filter_map(|held| landed.get(&held.id).cloned())
        .collect();
    wanted.extend(waiting.iter().filter_map(|held| request_of(held)));
    let times = commit_times(&repo.git, &wanted)?;
    // Each change that landed, with when; the ones in the cycle window are to_time from start.
    let ended: Vec<(&HeldChain, u64)> = chains
        .iter()
        .filter_map(|held| {
            let at = match landed.get(&held.id) {
                Some(publication) => times.get(publication).copied(),
                None => (held.record.state == ChangeState::Landed).then_some(held.committed),
            };
            at.map(|at| (held, at))
        })
        .collect();
    let recent: Vec<(&HeldChain, u64)> = ended
        .iter()
        .copied()
        .filter(|(_, at)| at.saturating_add(CYCLE_DAYS * DAY_S) >= now)
        .collect();
    let to_time: Vec<&HeldChain> = live
        .iter()
        .copied()
        .chain(recent.iter().map(|(held, _)| *held))
        .collect();
    let started = started(&repo.git, &to_time);
    let landed_count = ended
        .iter()
        .filter(|(_, at)| at.saturating_add(LANDED_DAYS * DAY_S) >= now)
        .count();
    let mut cycles: Vec<u64> = recent
        .iter()
        .map(|&(held, at)| at.saturating_sub(start_of(&started, held)))
        .collect();
    let recent_ids: BTreeSet<&str> = recent.iter().map(|(held, _)| held.id.as_str()).collect();
    let published = crate::log::published_of(&repo.git, &recent_ids)?;
    let oldest_live = live
        .iter()
        .map(|&held| (start_of(&started, held), held.id.clone()))
        .min()
        .map_or(
            Value::Null,
            |(start, change)| json!({"change": change, "age_s": now.saturating_sub(start)}),
        );
    let review = review_of(&waiting, &times, now);
    Ok(json!({
        "live": live.len(),
        "by_actor": by_actor(repo, trunk, &live)?,
        "landed": landed_count,
        "cycle_time_s": spread(&mut cycles, "landed"),
        "queue_wait_s": queue_wait(&published),
        "check_time_s": check_time(&published),
        "oldest_live": oldest_live,
        "waiting_approval": {
            "count": review.waiting,
            "oldest": review.oldest,
            "age_s": review.age_s,
        },
    }))
}

/// `by_actor` of [`flow`]: each actor's live changes, in name order, beside the WIP limit of the
/// policy at `trunk` ([`crate::policy::max_live_changes_at`], what `bay start` enforces). The
/// policy is read only when some change is live.
fn by_actor(repo: &Repo, trunk: &str, live: &[&HeldChain]) -> Result<Vec<Value>, String> {
    if live.is_empty() {
        return Ok(Vec::new());
    }
    let limit = crate::policy::max_live_changes_at(&repo.git, trunk)?;
    let mut counts: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
    for held in live {
        *counts.entry(held.record.actor.name.as_str()).or_insert(0) += 1;
    }
    Ok(counts
        .into_iter()
        .map(|(actor, count)| json!({"actor": actor, "live": count, "limit": limit}))
        .collect())
}

/// When `held` started, as [`started`] read it, or, when it read none, its chain head's time.
fn start_of(started: &HashMap<String, u64>, held: &HeldChain) -> u64 {
    started.get(&held.id).copied().unwrap_or(held.committed)
}

/// Whether the last operation of `held` waits for a person's approval.
fn awaits_approval(held: &HeldChain) -> bool {
    held.record
        .operations
        .last()
        .is_some_and(crate::publish::awaits_approval)
}

/// The state commit that carries the land request of the last operation of `held`.
fn request_of(held: &HeldChain) -> Option<String> {
    held.record.operations.last()?.request_id.clone()
}

/// The [`Review`] of `waiting`: each waits since its land request (`times`), or, when that time
/// is not read, since its chain head was written.
fn review_of(waiting: &[&HeldChain], times: &HashMap<String, u64>, now: u64) -> Review {
    let oldest = waiting
        .iter()
        .map(|&held| {
            let since = request_of(held)
                .and_then(|request| times.get(&request).copied())
                .unwrap_or(held.committed);
            (since, held.id.clone())
        })
        .min();
    Review {
        waiting: u64::try_from(waiting.len()).unwrap_or(u64::MAX),
        age_s: oldest.as_ref().map(|(since, _)| now.saturating_sub(*since)),
        oldest: oldest.map(|(_, change)| change),
    }
}

/// `queue_wait_s` of [`flow`]: the `queue_wait_s` each published terminal record in
/// `published` carries ([`crate::timing`]), the one home of an operation's queue wait.
fn queue_wait(published: &[Value]) -> Value {
    let mut waits: Vec<u64> = published
        .iter()
        .filter_map(|record| record["queue_wait_s"].as_u64())
        .collect();
    spread(&mut waits, "landed")
}

/// `check_time_s` of [`flow`]: the `seconds` of each attempt row (`checks[]`) the published
/// terminal records in `published` carry ([`crate::timing`]), one check run each.
fn check_time(published: &[Value]) -> Value {
    let mut runs: Vec<u64> = published
        .iter()
        .filter_map(|record| record["checks"].as_array())
        .flatten()
        .filter_map(|row| row["seconds"].as_u64())
        .collect();
    spread(&mut runs, "runs")
}

/// `{p50, p90, <count>}` of `values` in seconds, sorted here, `count` naming how many they
/// are; null when there are none.
fn spread(values: &mut [u64], count: &str) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    values.sort_unstable();
    let mut out = json!({
        "p50": percentile(values, 50),
        "p90": percentile(values, 90),
    });
    out[count] = json!(values.len());
    out
}

/// The `percent`th percentile of `sorted` (ascending, not empty), by nearest rank.
fn percentile(sorted: &[u64], percent: usize) -> u64 {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

/// The committer time of each of `commits` this repository holds, in Unix seconds, read by one
/// `git cat-file --batch`; a commit it does not hold is left out. No git call when `commits` is
/// empty.
///
/// # Errors
///
/// A git fault reading the batch.
pub fn commit_times(git: &Git, commits: &[String]) -> Result<HashMap<String, u64>, String> {
    let mut names: Vec<String> = commits.to_vec();
    names.sort();
    names.dedup();
    let texts = git.objects(&names).map_err(|error| error.message())?;
    Ok(names
        .into_iter()
        .zip(texts)
        .filter_map(|(name, text)| {
            let text = String::from_utf8(text?).ok()?;
            Some((name, committer_time(&text)?))
        })
        .collect())
}

/// The committer time of a commit object's text: the next to last word of its `committer`
/// header line (`committer <name> <<email>> <time> <zone>`).
fn committer_time(text: &str) -> Option<u64> {
    text.lines()
        .take_while(|line| !line.is_empty())
        .find_map(|line| line.strip_prefix("committer "))
        .and_then(|line| line.split_whitespace().rev().nth(1))
        .and_then(|time| time.parse().ok())
}

/// When each change of `chains` started: the earliest committer time on its chain head's
/// first-parent line above its first base (`base_first`), read by one first-parent walk of
/// every head at once, the bases excluded. A change whose line cannot be read is left out, and
/// a walk git refuses (an object missing here) leaves every one out: [`flow`] then times them
/// from their chain head.
fn started(git: &Git, chains: &[&HeldChain]) -> HashMap<String, u64> {
    if chains.is_empty() {
        return HashMap::new();
    }
    let mut input = String::new();
    for held in chains {
        input.push_str(&held.head);
        input.push('\n');
    }
    input.push_str("--not\n");
    for held in chains {
        input.push_str(&held.record.base_first);
        input.push('\n');
    }
    let Ok(out) = git.run_with(
        &["log", "--first-parent", "--format=%H %ct %P", "--stdin"],
        &[],
        Some(input.as_bytes()),
    ) else {
        return HashMap::new();
    };
    // Each commit the walk saw: its time and its first parent.
    let line: HashMap<&str, (u64, Option<&str>)> = out
        .lines()
        .filter_map(|entry| {
            let mut words = entry.split_whitespace();
            let commit = words.next()?;
            let time = words.next()?.parse().ok()?;
            Some((commit, (time, words.next())))
        })
        .collect();
    chains
        .iter()
        .filter_map(|held| {
            let mut at = Some(held.head.as_str());
            let mut earliest = None;
            // Each commit at most once, so a line that loops (it cannot) still ends.
            for _ in 0..=line.len() {
                let Some((time, parent)) = at.and_then(|commit| line.get(commit)) else {
                    break;
                };
                earliest = Some(earliest.map_or(*time, |seen: u64| seen.min(*time)));
                at = *parent;
            }
            earliest.map(|time| (held.id.clone(), time))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{committer_time, percentile};

    #[test]
    fn a_percentile_is_by_nearest_rank() {
        assert_eq!(percentile(&[7], 50), 7);
        assert_eq!(percentile(&[7], 90), 7);
        assert_eq!(percentile(&[1, 2, 3, 4], 50), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 90), 4);
        let ten: Vec<u64> = (1..=10).collect();
        assert_eq!(percentile(&ten, 50), 5);
        assert_eq!(percentile(&ten, 90), 9);
    }

    #[test]
    fn the_committer_time_is_read_from_the_header() {
        let text = "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor bay <bay@bay.invalid> 100 +0000\ncommitter bay <bay@bay.invalid> 1700000000 +0100\n\ncommitter 5 +0000\n";
        assert_eq!(committer_time(text), Some(1_700_000_000));
        assert_eq!(committer_time("tree x\n\nbody\n"), None);
    }
}

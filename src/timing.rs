//! The timing of one operation, for its terminal record (`ROADMAP` § 3: queue wait, separately
//! from check time).
//!
//! Every time is a commit time in Unix seconds, read from a commit bay already wrote and signed;
//! no wall clock is read here. Each field names its source:
//!
//! - `submitted_at`: the land request commit (agent-signed).
//! - `queued_at`: the first chain state (executor-signed) that records the operation holding a
//!   queue place (`queued <op> at <seq>`). Absent when it never queued.
//! - `head_at`: the first claim (`claim <op> epoch <n>`, the state that raised its epoch) after
//!   `queued_at`; the first claim when it never queued.
//! - `checks[]`: one row per attempt. `started_at` is its candidate commit's; `finished_at` the
//!   chain state that first records the attempt checked or blocked. The last attempt's row also
//!   holds `runs[]`, the evidence records on its candidate, each with the worker wall time bay
//!   measured around the check (`seconds`).
//! - `published_at` (outcome `published`): the chain state `landed <op>`, written right after
//!   the trunk push. `ended_at` (any other outcome): the first chain state that records the
//!   operation in a terminal state.
//! - `attempts`: how many candidates were checked.
//!
//! Derived, each only when its inputs are present: `queue_wait_s` is `head_at` less `queued_at`
//! (less `submitted_at` when it never queued); `check_s` the sum of every attempt's
//! `finished_at` less `started_at`; `total_s` the end less `submitted_at`.
//!
//! The cost is a constant number of `git` spawns, however many heartbeats the chain holds: one
//! `git log` over the chain's first parents (commit ids, times and subjects), one `git log` of
//! the request and candidate commits' times, the last candidate's evidence (two), and, for an
//! outcome other than `published` only, one `cat-file --batch` of the few records that may
//! hold the terminal state.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::change::{ChangeRecord, Operation, RECORD_FILE};
use crate::git::Git;

/// The timing fields a terminal record may carry, as `bay status` copies them onto the
/// operation row.
pub const FIELDS: &[&str] = &[
    "submitted_at",
    "queued_at",
    "head_at",
    "published_at",
    "ended_at",
    "attempts",
    "checks",
    "queue_wait_s",
    "check_s",
    "total_s",
];

/// The most chain states read back from the head.
const MAX_STATES: usize = 4096;

/// One chain state: its commit, commit time and subject.
struct State {
    commit: String,
    at: u64,
    subject: String,
}

/// The chain states from the request of `op` to `head`, oldest first, in one `git log` over
/// the first parents. The walk stops at the operation's revision, so it never reads trunk.
fn states(git: &Git, head: &str, op: &Operation) -> Vec<State> {
    let max = format!("--max-count={MAX_STATES}");
    let not = format!("^{}", op.revision);
    let Ok(text) = git.run(&[
        "log",
        "--first-parent",
        &max,
        "--format=%H%x09%ct%x09%s",
        head,
        &not,
    ]) else {
        return Vec::new();
    };
    let request = format!("request {}", op.op_id);
    let mut out = Vec::new();
    for line in text.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(commit), Some(Ok(at)), subject) = (
            fields.next(),
            fields.next().map(str::parse::<u64>),
            fields.next().unwrap_or_default(),
        ) else {
            continue;
        };
        out.push(State {
            commit: commit.to_owned(),
            at,
            subject: subject.to_owned(),
        });
        if subject == request {
            break;
        }
    }
    out.reverse();
    out
}

/// The committer time of each of `commits`, in one `git log`; a commit this store lacks is
/// left out.
fn commit_times(git: &Git, commits: &[&str]) -> HashMap<String, u64> {
    if commits.is_empty() {
        return HashMap::new();
    }
    let mut args = vec![
        "log",
        "--no-walk=unsorted",
        "--ignore-missing",
        "--format=%H %ct",
    ];
    args.extend(commits);
    git.run(&args)
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

/// The change record of each of `states`, in one `cat-file --batch`; `None` for one that
/// cannot be read.
fn records(git: &Git, states: &[&State]) -> Vec<Option<ChangeRecord>> {
    if states.is_empty() {
        return Vec::new();
    }
    let mut input = String::new();
    for state in states {
        input.push_str(&state.commit);
        input.push(':');
        input.push_str(RECORD_FILE);
        input.push('\n');
    }
    let Ok(bytes) = git.run_bytes(&["cat-file", "--batch"], &[], Some(input.as_bytes())) else {
        return states.iter().map(|_| None).collect();
    };
    let mut rest = bytes.as_slice();
    let mut out = Vec::with_capacity(states.len());
    for _ in states {
        let Some(end) = rest.iter().position(|byte| *byte == b'\n') else {
            out.push(None);
            continue;
        };
        let header = String::from_utf8_lossy(&rest[..end]).into_owned();
        rest = &rest[end + 1..];
        // `<oid> <type> <size>`, or `<name> missing`.
        let fields: Vec<&str> = header.split(' ').collect();
        let size = match fields[..] {
            [_, "blob", size] => size.parse::<usize>().ok(),
            _ => None,
        };
        let Some(size) = size else {
            out.push(None);
            continue;
        };
        let body = rest.get(..size).unwrap_or_default();
        rest = rest.get(size + 1..).unwrap_or_default();
        out.push(serde_json::from_slice(body).ok());
    }
    out
}

/// The commit time of the first state that records `op_id` in a terminal state, for an
/// outcome other than a publication: read from the records of only the states whose subject
/// is not a claim, a queue place or a heartbeat.
fn ended_at(git: &Git, states: &[State], op_id: &str) -> Option<u64> {
    let routine = [
        format!("claim {op_id} "),
        format!("queued {op_id} "),
        format!("heartbeat {op_id} "),
    ];
    let candidates: Vec<&State> = states
        .iter()
        .filter(|state| {
            !routine
                .iter()
                .any(|prefix| state.subject.starts_with(prefix))
        })
        .collect();
    candidates
        .iter()
        .zip(records(git, &candidates))
        .find(|(_, record)| {
            record.as_ref().is_some_and(|record| {
                record
                    .operations
                    .iter()
                    .any(|op| op.op_id == op_id && op.state.is_terminal())
            })
        })
        .map(|(state, _)| state.at)
}

/// One row per attempt of `op`, and the sum of their check seconds (`None` when one cannot be
/// read).
fn checks(
    git: &Git,
    op: &Operation,
    states: &[State],
    times: &HashMap<String, u64>,
) -> (Vec<Value>, Option<u64>) {
    let mut check_s = Some(0_u64);
    let mut rows = Vec::with_capacity(op.attempts.len());
    for (index, attempt) in op.attempts.iter().enumerate() {
        let started_at = times.get(&attempt.commit).copied();
        let done = [
            format!("checked {}", attempt.candidate_id),
            format!("blocked {}", attempt.candidate_id),
        ];
        let finished_at = states
            .iter()
            .find(|state| done.contains(&state.subject))
            .map(|state| state.at);
        let seconds = started_at
            .zip(finished_at)
            .map(|(started, finished)| finished.saturating_sub(started));
        check_s = check_s.zip(seconds).map(|(sum, seconds)| sum + seconds);
        let mut row = json!({
            "candidate": attempt.candidate_id,
            "started_at": started_at,
            "finished_at": finished_at,
            "seconds": seconds,
            "verdict": attempt.verdict,
        });
        // Only the last candidate's evidence is read: two spawns, however many attempts.
        if index + 1 == op.attempts.len() {
            let runs: Vec<Value> = crate::evidence::records_for(git, &attempt.tree)
                .unwrap_or_default()
                .into_iter()
                .filter(|record| {
                    record.kind == crate::evidence::KIND_CHECK
                        && record.subject.commit == attempt.commit
                })
                .map(|record| {
                    json!({
                        "check": record.check.id,
                        "environment": record.check.environment,
                        "producer": record.producer.name,
                        "verdict": record.verdict,
                        "seconds": record.seconds,
                    })
                })
                .collect();
            row["runs"] = json!(runs);
        }
        rows.push(row);
    }
    (rows, check_s)
}

/// The timing of operation `op`, read from the chain ending at `head`, as the fields of its
/// terminal record with `outcome`. Fields whose source cannot be read are left out.
#[must_use]
pub fn of(git: &Git, head: &str, op: &Operation, outcome: &str) -> Map<String, Value> {
    let states = states(git, head, op);
    let op_id = op.op_id.as_str();
    let queued_prefix = format!("queued {op_id} at ");
    let queued = states
        .iter()
        .position(|state| state.subject.starts_with(&queued_prefix));
    let claim = format!("claim {op_id} epoch ");
    let head_at = states
        .iter()
        .enumerate()
        .find(|(index, state)| {
            state.subject.starts_with(&claim) && queued.is_none_or(|queued| *index > queued)
        })
        .map(|(_, state)| state.at);
    let queued_at = queued.map(|index| states[index].at);

    let mut commits: Vec<&str> = op.request_id.iter().map(String::as_str).collect();
    commits.extend(op.attempts.iter().map(|attempt| attempt.commit.as_str()));
    let times = commit_times(git, &commits);
    let submitted_at = op
        .request_id
        .as_ref()
        .and_then(|request| times.get(request).copied());
    let (end_key, end_at) = if outcome == "published" {
        let landed = format!("landed {op_id}");
        let at = states
            .iter()
            .find(|state| state.subject == landed)
            .map(|state| state.at);
        ("published_at", at)
    } else {
        ("ended_at", ended_at(git, &states, op_id))
    };
    let (rows, check_s) = checks(git, op, &states, &times);

    let mut out = Map::new();
    let mut put = |key: &str, value: Option<u64>| {
        if let Some(value) = value {
            out.insert(key.to_owned(), json!(value));
        }
    };
    put("submitted_at", submitted_at);
    put("queued_at", queued_at);
    put("head_at", head_at);
    put(end_key, end_at);
    put(
        "queue_wait_s",
        head_at
            .zip(queued_at.or(submitted_at))
            .map(|(head, from)| head.saturating_sub(from)),
    );
    put("check_s", check_s);
    put(
        "total_s",
        end_at
            .zip(submitted_at)
            .map(|(end, from)| end.saturating_sub(from)),
    );
    out.insert("attempts".to_owned(), json!(op.attempts.len()));
    out.insert("checks".to_owned(), json!(rows));
    out
}

/// The timing fields terminal record `record` carries, as one object; `None` on a record
/// written before they were kept.
#[must_use]
pub fn from_terminal(record: &Value) -> Option<Value> {
    let object = record.as_object()?;
    let timing: Map<String, Value> = FIELDS
        .iter()
        .filter_map(|field| {
            object
                .get(*field)
                .map(|value| ((*field).to_owned(), value.clone()))
        })
        .collect();
    (!timing.is_empty()).then_some(Value::Object(timing))
}

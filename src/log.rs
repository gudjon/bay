//! Terminal records (`DESIGN` § 5, § 9a): `refs/bay/log/<executor>`, one append-only
//! chain per executor, no shared tip. Each commit's tree holds `record.json`, a compact
//! terminal record: op id, request digest, outcome, published commit, evidence and policy ids.
//! It outlives the change's drafts, so `status` explains an outcome after retention and gc.
//! The same log holds the `bay.event/1` events trunk cannot derive (`INTERFACES` § 5): an
//! event is appended once, by id, and only then is the notify adapter told (ruling R6).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::change::RECORD_IDENTITY;
use crate::git::Git;
use crate::repo::Repo;
use crate::signing::Signer;

pub const LOG_PREFIX: &str = crate::names::LOG;
/// An event trunk cannot derive (`INTERFACES` § 5), kept in the same log.
pub const EVENT_SCHEMA: &str = "bay.event/1";

#[must_use]
pub fn log_ref(executor: &str) -> String {
    format!("{LOG_PREFIX}{executor}")
}

/// Fetches every executor's log from the remote (best effort: offline keeps what is local).
pub fn fetch(repo: &Repo) {
    let refspec = format!("+{LOG_PREFIX}*:{LOG_PREFIX}*");
    // A10 (the no-work-is-lost rule, 2026-10-27): this executor's own log is never forced, so a
    // record whose push has not reached the remote yet is never fetched over; the remote's
    // moves it only forward.
    let own = log_ref(&crate::signing::Keys::load(&repo.git).executor_name);
    let _ = repo.git.run(&[
        "fetch",
        "-q",
        "--no-tags",
        repo.remote(),
        &refspec,
        &format!("^{own}"),
    ]);
    let _ = repo.git.run(&[
        "fetch",
        "-q",
        "--no-tags",
        repo.remote(),
        &format!("{own}:{own}"),
    ]);
}

/// Appends `record` to this executor's log and pushes it. Only this executor writes its log.
///
/// # Errors
///
/// A git fault while writing locally; a failed push is retried by the next append.
pub fn append(
    repo: &Repo,
    executor: &str,
    signer: Option<&Signer>,
    record: &Value,
) -> Result<String, String> {
    let git = &repo.git;
    let name = log_ref(executor);
    let _ = git.run(&[
        "fetch",
        "-q",
        "--no-tags",
        repo.remote(),
        &format!("{name}:{name}"),
    ]);
    let mut json = serde_json::to_string_pretty(record).map_err(|error| error.to_string())?;
    json.push('\n');
    let blob = git
        .run_with(
            &["hash-object", "-w", "--stdin"],
            &[],
            Some(json.as_bytes()),
        )
        .map_err(|error| error.message())?;
    let entry = format!("100644 blob {blob}\trecord.json\n");
    let tree = git
        .run_with(&["mktree"], &[], Some(entry.as_bytes()))
        .map_err(|error| error.message())?;
    let parent = git.ref_target(&name).map_err(|error| error.message())?;
    let mut args: Vec<String> = Vec::new();
    if let Some(signer) = signer {
        args.extend(signer.config_args());
    }
    args.extend(["commit-tree".to_owned(), tree]);
    if let Some(parent) = &parent {
        args.extend(["-p".to_owned(), parent.clone()]);
    }
    args.push(
        if signer.is_some() {
            "-S"
        } else {
            "--no-gpg-sign"
        }
        .to_owned(),
    );
    let subject = if record["schema"] == EVENT_SCHEMA {
        format!(
            "event {} {}",
            record["kind"].as_str().unwrap_or_default(),
            record["id"].as_str().unwrap_or_default()
        )
    } else {
        format!(
            "terminal record {}",
            record["op_id"].as_str().unwrap_or_default()
        )
    };
    args.extend(["-m".to_owned(), subject]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let identity: Vec<(String, String)> = signer.map_or_else(
        || {
            RECORD_IDENTITY
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        },
        Signer::identity,
    );
    let envs: Vec<(&str, &str)> = identity
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let commit = git
        .run_with(&args, &envs, None)
        .map_err(|error| error.message())?;
    let update = match &parent {
        Some(parent) => format!("update {name} {commit} {parent}\n"),
        None => format!("create {name} {commit}\n"),
    };
    git.run_with(&["update-ref", "--stdin"], &[], Some(update.as_bytes()))
        .map_err(|error| error.message())?;
    let _ = git.run(&["push", "-q", repo.remote(), &format!("{name}:{name}")]);
    Ok(commit)
}

/// Appends the `bay.event/1` record `event` to this executor's log unless a log already holds
/// its `id` ([`held_events`]). Returns whether the event was new. Only a new event is told to
/// the notify adapter, by the caller and after this returns, so the record is written before
/// anyone is told and a lost notice never loses the event (ruling R6).
///
/// # Errors
///
/// A git fault while writing locally.
pub fn append_event(
    repo: &Repo,
    executor: &str,
    signer: Option<&Signer>,
    event: &Value,
) -> Result<bool, String> {
    let id = event["id"].as_str().unwrap_or_default();
    fetch(repo);
    if held_events(&repo.git)?.contains(id) {
        return Ok(false);
    }
    append(repo, executor, signer, event)?;
    Ok(true)
}

/// The id of every event the executor logs in this repository hold, read from the log
/// commits' subjects (`event <kind> <id>`) in one `git log`: no record is read.
///
/// # Errors
///
/// A git fault.
pub fn held_events(git: &Git) -> Result<BTreeSet<String>, String> {
    let tips = tips(git)?;
    if tips.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut args = vec!["log".to_owned(), "--format=%s".to_owned()];
    args.extend(tips.into_values());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let subjects = git.run(&args).map_err(|error| error.message())?;
    Ok(subjects
        .lines()
        .filter_map(|subject| subject.strip_prefix("event "))
        .filter_map(|rest| rest.split_whitespace().nth(1))
        .map(str::to_owned)
        .collect())
}

/// Pushes this executor's log, best effort, so a record whose push failed reaches the remote
/// without waiting for the next append.
pub fn push(repo: &Repo, executor: &str) {
    let name = log_ref(executor);
    if repo.git.ref_target(&name).ok().flatten().is_some() {
        let _ = repo
            .git
            .run(&["push", "-q", repo.remote(), &format!("{name}:{name}")]);
    }
}

/// The tip of every executor log this repository holds, by executor name.
///
/// # Errors
///
/// A git fault.
pub fn tips(git: &Git) -> Result<BTreeMap<String, String>, String> {
    let listed = git
        .run(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            LOG_PREFIX,
        ])
        .map_err(|error| error.message())?;
    Ok(listed
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, tip)| {
            (
                name.trim_start_matches(LOG_PREFIX).to_owned(),
                tip.to_owned(),
            )
        })
        .collect())
}

/// What [`events_since`] read: the events, the log tips a later read goes on from, and whether
/// the limit left events unread.
#[derive(Debug, Default)]
pub struct Events {
    pub events: Vec<Value>,
    pub tips: BTreeMap<String, String>,
    pub more: bool,
}

/// The `bay.event/1` records appended to the logs at `tips` after the tips in `since`, oldest
/// first per log, at most `limit` in all; a log `since` does not name, or names by a commit
/// this repository lacks, is read from its start. A log whose tip is its tip in `since` costs no
/// git call. Only event commits are read, found by their subjects in one `git log` per log that
/// moved, and their records in one `cat-file --batch`. A log the limit stopped has, in the
/// result's tips, the last event read from it (or where it was read from), so the next read goes
/// on from there.
///
/// # Errors
///
/// A git fault.
pub fn events_since(
    git: &Git,
    since: &BTreeMap<String, String>,
    tips: &BTreeMap<String, String>,
    limit: usize,
) -> Result<Events, String> {
    let mut out = Events::default();
    for (executor, tip) in tips {
        if since.get(executor) == Some(tip) {
            out.tips.insert(executor.clone(), tip.clone());
            continue;
        }
        let from = since.get(executor).filter(|from| {
            git.run(&["cat-file", "-e", &format!("{from}^{{commit}}")])
                .is_ok()
        });
        let mut args = vec![
            "log".to_owned(),
            "--reverse".to_owned(),
            "--format=%H %s".to_owned(),
            tip.clone(),
        ];
        if let Some(from) = from {
            args.push(format!("^{from}"));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let commits = git.run(&args).map_err(|error| error.message())?;
        let events: Vec<&str> = commits
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(_, subject)| subject.starts_with("event "))
            .map(|(commit, _)| commit)
            .collect();
        let room = limit.saturating_sub(out.events.len());
        let stopped = events.len() > room;
        let taken = &events[..events.len().min(room)];
        for text in records(git, taken)?.into_iter().flatten() {
            if let Ok(record) = serde_json::from_str::<Value>(&text)
                && record["schema"] == EVENT_SCHEMA
            {
                out.events.push(record);
            }
        }
        out.more |= stopped;
        let reached = if stopped {
            taken
                .last()
                .map(|commit| (*commit).to_owned())
                .or_else(|| from.cloned())
        } else {
            Some(tip.clone())
        };
        if let Some(reached) = reached {
            out.tips.insert(executor.clone(), reached);
        }
    }
    Ok(out)
}

/// How many of the newest events of each log [`wake_failing`] reads.
const WAKE_WINDOW: usize = 40;

/// How many attempted wakes, at least, must all have failed for [`wake_failing`] to warn.
const WAKE_FAILED_AT_LEAST: usize = 3;

/// R11 (2026-10-27: 0 of 102 wakes reached a seat in a day, and nothing showed it): a
/// `wake_failing` warning when every wake attempted among the newest events of the logs this
/// repository holds failed, and there were at least [`WAKE_FAILED_AT_LEAST`]. It names how many
/// failed, the newest failure's cause (`why`, when its runner recorded one) and its change.
/// `None` when a wake got through, too few were attempted, or nothing can be read.
#[must_use]
pub fn wake_failing(git: &Git) -> Option<Value> {
    let tips = tips(git).ok()?;
    let mut wakes: Vec<(String, Value)> = Vec::new();
    for tip in tips.values() {
        let listed = git
            .run(&["log", &format!("-{WAKE_WINDOW}"), "--format=%H %s", tip])
            .ok()?;
        let events: Vec<&str> = listed
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(_, subject)| subject.starts_with("event "))
            .map(|(commit, _)| commit)
            .collect();
        for text in records(git, &events).ok()?.into_iter().flatten() {
            if let Ok(record) = serde_json::from_str::<Value>(&text)
                && record["wake"].is_object()
                && record["wake"]["result"] != "not_attempted"
            {
                let at = record["at"].as_str().unwrap_or_default().to_owned();
                wakes.push((at, record));
            }
        }
    }
    if wakes.len() < WAKE_FAILED_AT_LEAST
        || wakes
            .iter()
            .any(|(_, record)| record["wake"]["result"] != "uncertain")
    {
        return None;
    }
    let (_, newest) = wakes.iter().max_by(|a, b| a.0.cmp(&b.0))?;
    Some(serde_json::json!({
        "code": "wake_failing",
        "failed": wakes.len(),
        "why": newest["wake"]["why"],
        "change": newest["change"],
        "at": newest["at"],
        "note": "every recent wake of a change's author failed, so no one is told of refusals: run bay plugin test wake",
    }))
}

/// The `record.json` of each of `commits`, read in one `cat-file --batch`: `None` for a commit
/// with no such blob, or one that is not UTF-8.
fn records(git: &Git, commits: &[&str]) -> Result<Vec<Option<String>>, String> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    let input: String = commits
        .iter()
        .map(|commit| [commit, ":record.json\n"].concat())
        .collect();
    let bytes = git
        .run_bytes(&["cat-file", "--batch"], &[], Some(input.as_bytes()))
        .map_err(|error| error.message())?;
    let short = || "cat-file --batch: a short answer".to_owned();
    let mut rest = bytes.as_slice();
    let mut out = Vec::with_capacity(commits.len());
    for _ in commits {
        let end = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(short)?;
        let header = String::from_utf8_lossy(&rest[..end]).into_owned();
        rest = &rest[end + 1..];
        // `<oid> <type> <size>`, or `<name> missing` for an object the commit lacks.
        let fields: Vec<&str> = header.split(' ').collect();
        let [_, kind, size] = fields[..] else {
            out.push(None);
            continue;
        };
        let size: usize = size
            .parse()
            .map_err(|_| format!("cat-file --batch: {header}"))?;
        let body = rest.get(..size).ok_or_else(short)?;
        rest = rest.get(size + 1..).ok_or_else(short)?;
        out.push(
            (kind == "blob")
                .then(|| String::from_utf8(body.to_vec()).ok())
                .flatten(),
        );
    }
    Ok(out)
}

/// G316: the published terminal records of `changes`, one per operation, from every executor
/// log this repository holds, in three `git` spawns however long the logs are: one
/// `for-each-ref` of the logs, one `git log` of their subjects, and one `cat-file --batch` of
/// only the records whose subject names an operation of one of `changes`
/// (`terminal record <change>.<n>`). No git call when `changes` is empty.
///
/// # Errors
///
/// A git fault.
pub fn published_of(git: &Git, changes: &BTreeSet<&str>) -> Result<Vec<Value>, String> {
    if changes.is_empty() {
        return Ok(Vec::new());
    }
    let tips = tips(git)?;
    if tips.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["log".to_owned(), "--format=%H %s".to_owned()];
    args.extend(tips.into_values());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let listed = git.run(&args).map_err(|error| error.message())?;
    let commits: Vec<&str> = listed
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, subject)| {
            subject
                .strip_prefix("terminal record ")
                .and_then(|op_id| op_id.rsplit_once('.'))
                .is_some_and(|(change, _)| changes.contains(change))
        })
        .map(|(commit, _)| commit)
        .collect();
    let mut seen = BTreeSet::new();
    Ok(records(git, &commits)?
        .into_iter()
        .flatten()
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(|record| record["schema"] != EVENT_SCHEMA && record["outcome"] == "published")
        .filter(|record| seen.insert(record["op_id"].to_string()))
        .collect())
}

/// Every terminal record for change `id`, from every executor log this repository holds.
///
/// # Errors
///
/// A git fault.
pub fn records_for(git: &Git, id: &str) -> Result<Vec<Value>, String> {
    every_record(git, &|record| {
        record["change"] == id && record["schema"] != EVENT_SCHEMA
    })
}

fn every_record(git: &Git, keep: &dyn Fn(&Value) -> bool) -> Result<Vec<Value>, String> {
    Ok(all_records(git)?
        .iter()
        .filter(|record| keep(record))
        .cloned()
        .collect())
}

/// Every record of every executor log, read once per state of the logs: one `cat-file --batch`
/// over all their commits, kept while the logs' tips stay where they were. `bay status` asks
/// for each change's records in turn, and before it read every log once per change, one
/// process per commit (W5: 22,533 reads on a queue of 87 changes).
fn all_records(git: &Git) -> Result<std::sync::Arc<Vec<Value>>, String> {
    use std::sync::{Arc, Mutex};
    static READ: Mutex<Option<(String, Arc<Vec<Value>>)>> = Mutex::new(None);
    let tips = git
        .run(&[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            LOG_PREFIX,
        ])
        .map_err(|error| error.message())?;
    let key = format!("{}\n{tips}", git.dir().display());
    if let Some((read, records)) = READ.lock().ok().as_deref().and_then(Option::as_ref)
        && *read == key
    {
        return Ok(Arc::clone(records));
    }
    let mut out = Vec::new();
    for name in tips
        .lines()
        .filter_map(|line| line.split_once(' ').map(|(_, name)| name))
    {
        let commits = git
            .run(&["rev-list", name])
            .map_err(|error| error.message())?;
        let commits: Vec<&str> = commits.lines().collect();
        // G104b: a record the commit holds and bay cannot read is an error, never skipped.
        for text in records(git, &commits)?.into_iter().flatten() {
            if let Ok(record) = serde_json::from_str::<Value>(&text) {
                out.push(record);
            }
        }
    }
    let records = Arc::new(out);
    if let Ok(mut read) = READ.lock() {
        *read = Some((key, Arc::clone(&records)));
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{LOG_PREFIX, events_since, held_events};
    use crate::change::RECORD_IDENTITY;
    use crate::git::Git;
    use serde_json::json;

    /// Appends one commit holding `record` to log `a`, under `subject`, as `append` writes it.
    fn add(git: &Git, record: &serde_json::Value, subject: &str) -> String {
        let blob = git
            .run_with(
                &["hash-object", "-w", "--stdin"],
                &[],
                Some(record.to_string().as_bytes()),
            )
            .unwrap();
        let entry = format!("100644 blob {blob}\trecord.json\n");
        let tree = git
            .run_with(&["mktree"], &[], Some(entry.as_bytes()))
            .unwrap();
        let name = format!("{LOG_PREFIX}a");
        let parent = git.ref_target(&name).unwrap();
        let mut args = vec!["commit-tree", &tree, "--no-gpg-sign", "-m", subject];
        if let Some(parent) = &parent {
            args.extend(["-p", parent]);
        }
        let commit = git.run_with(&args, RECORD_IDENTITY, None).unwrap();
        git.run(&["update-ref", &name, &commit]).unwrap();
        commit
    }

    #[test]
    fn a_limit_stops_the_read_where_the_next_one_goes_on() {
        let dir = std::env::temp_dir().join(format!("bay-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = Git::new(&dir);
        git.run(&["init", "-q"]).unwrap();
        let event = |n: u32| json!({"schema": "bay.event/1", "id": format!("evt_{n}")});
        let first = add(&git, &event(1), "event refused evt_1");
        add(&git, &json!({"op_id": "c.1"}), "terminal record c.1");
        let second = add(&git, &event(2), "event refused evt_2");
        add(&git, &event(3), "event refused evt_3");
        let tips = super::tips(&git).unwrap();

        let read = events_since(&git, &BTreeMap::new(), &tips, 2).unwrap();
        let ids: Vec<&str> = read
            .events
            .iter()
            .map(|event| event["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["evt_1", "evt_2"]);
        assert!(read.more);
        assert_eq!(read.tips["a"], second);
        let rest = events_since(&git, &read.tips, &tips, 2).unwrap();
        assert_eq!(rest.events.len(), 1);
        assert_eq!(rest.events[0]["id"], "evt_3");
        assert!(!rest.more);
        assert_eq!(rest.tips, tips);
        let since = [("a".to_owned(), first.clone())].into();
        let none = events_since(&git, &since, &tips, 0).unwrap();
        assert!(none.more);
        assert_eq!(none.events.len(), 0);
        assert_eq!(none.tips["a"], first);

        let held: Vec<String> = held_events(&git).unwrap().into_iter().collect();
        assert_eq!(held, ["evt_1", "evt_2", "evt_3"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_that_did_not_move_costs_no_git_call() {
        // A hook reads the logs at every step: one whose tip is where the read stopped is not
        // asked again. A repository that is not there answers no git call.
        let git = Git::new(std::env::temp_dir().join("bay-log-no-such-repository"));
        let tips: BTreeMap<String, String> = [("a".to_owned(), "1".repeat(40))].into();
        let read = events_since(&git, &tips, &tips, 100).unwrap();
        assert!(read.events.is_empty() && !read.more);
        assert_eq!(read.tips, tips);
    }
}

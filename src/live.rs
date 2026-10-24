//! The machine's live summary ref, `refs/bay/live/<machine>` (`DESIGN` § 11a, § 10a, § 4;
//! ruling R1).
//!
//! A machine publishes its sessions so other machines see them by an ordinary fetch. The ref
//! holds one parentless commit whose tree is `live.json` alone (schema `bay.live/1`): the
//! machine's name, the time it was written, the fingerprint of the agent key that signs it, and
//! one row per live session. A row is a projection of the session's latest work record line
//! (`INTERFACES` § 5a): session, when it started (G166), actor, change, state, event, agent,
//! context use, whether its view holds uncheckpointed edits (ruling R7) and time. Local paths
//! (repository, view, files) stay in the local record. A row whose time is older than the takeover window is reaped: a session
//! that died without `SessionEnd` stops counting once its own heartbeat is that old, however
//! often a sibling session on the machine fires. Each write
//! replaces the ref with a new parentless commit; the host's reflog keeps its history. With a
//! key registry the commit is signed by this box's agent key; the host hook binds the ref to
//! its first key, and one key to one live ref.
//!
//! The machine name is repository-local `git config bay.machine` (install writes it), by default
//! the host name folded to `[a-z0-9-]{1,32}`, and is an operator name (§ 10a).
//!
//! A hook event never pushes and never waits: it starts one detached `bay live push` unless one
//! is running. The pusher holds a lock file for its whole life, pushes, waits one heartbeat
//! (`[runner] heartbeat_s`) and pushes again while session records changed in the meantime,
//! so a burst of events ends with its latest state published, pushes stay one per heartbeat,
//! and two pushers never overwrite a newer summary with an older one. Its last outcome is kept
//! in `<git-common-dir>/bay/live-push.json`, which `bay doctor` shows when it failed.
//!
//! G314: the summary also carries `doctor`, this machine's last `bay doctor` verdict
//! ([`DOCTOR_FIELDS`]): doctor writes and pushes the summary with it ([`publish_doctor`]), and
//! every later write carries the block forward from the local live ref, so the ref is the one
//! home of the machine's readiness. A machine with a verdict and no session records still
//! publishes its summary, with no sessions.
//!
//! § 4: a heartbeat naming a change counts as that change's activity only when it is signed by
//! the actor the change chain records as its current actor, and its row of the change is fresh
//! ([`heartbeat_of`]).

use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::git::{Git, GitError};
use crate::registry::{KeyClass, Registry};
use crate::repo::Repo;
use crate::result::BayResult;
use crate::signing::{self, Keys};

pub const LIVE_PREFIX: &str = crate::names::LIVE;
pub const LIVE_SCHEMA: &str = "bay.live/1";
pub const LIVE_FILE: &str = "live.json";
/// Repository-local Git config naming this machine.
pub const MACHINE_KEY: &str = "bay.machine";
/// The largest `live.json` the host accepts.
pub const LIVE_MAX_BYTES: u64 = 32 * 1024;
/// The most rows a summary carries: the newest ones.
const LIVE_MAX_ROWS: usize = 64;
/// The fields of a work record line a row keeps.
const ROW_FIELDS: [&str; 13] = [
    "session",
    "started",
    "actor",
    "actor_source",
    "permission_mode",
    "change",
    "state",
    "event",
    "agent",
    "context",
    "uncheckpointed",
    "uncheckpointed_unknown",
    "at",
];
/// The field of a summary that carries this machine's last `bay doctor` verdict (G314).
pub const DOCTOR_FIELD: &str = "doctor";
/// The fields of a `doctor` block: no path and no secret, only names, states and versions.
pub const DOCTOR_FIELDS: [&str; 8] = [
    "at",
    "bay_version",
    "decision",
    "reason",
    "red",
    "hooks",
    "sandbox",
    "role",
];
/// The most of a session record's tail read for its latest line.
const TAIL_BYTES: u64 = 64 * 1024;
/// The most pushes one pusher makes before it exits.
const MAX_ROUNDS: u32 = 6;

#[must_use]
pub fn live_ref(machine: &str) -> String {
    format!("{LIVE_PREFIX}{machine}")
}

/// A host name folded to an operator name: lower case, every other character `-`, at most 32
/// characters, no leading or trailing `-`; `machine` when nothing is left.
#[must_use]
pub fn fold(host: &str) -> String {
    let folded: String = host
        .trim()
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    let folded = folded.trim_matches('-');
    if folded.is_empty() {
        "machine".to_owned()
    } else {
        folded.to_owned()
    }
}

fn hostname() -> String {
    let mut cmd = Command::new("hostname");
    cmd.stdin(Stdio::null());
    crate::git::run_bounded(cmd, "hostname")
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn configured(git: &Git) -> Option<String> {
    git.config_get(MACHINE_KEY)
        .ok()
        .flatten()
        .filter(|name| !name.is_empty())
}

/// This machine's name: `bay.machine`, else the folded host name.
///
/// # Errors
///
/// `bay.machine` is not an operator name.
pub fn machine(git: &Git) -> Result<String, String> {
    let name = configured(git).unwrap_or_else(|| fold(&hostname()));
    crate::names::operator_name(&name).map_err(|why| format!("{MACHINE_KEY}: {why}"))?;
    Ok(name)
}

/// Install's write: sets `bay.machine` in the repository's local config when it is unset.
///
/// # Errors
///
/// The configured name is not an operator name, or Git cannot write the config.
pub fn ensure_machine(git: &Git) -> Result<String, String> {
    let name = machine(git)?;
    if configured(git).is_none() {
        git.run(&["config", "--local", MACHINE_KEY, &name])
            .map_err(|error| error.message())?;
    }
    Ok(name)
}

fn sessions_dir(repo: &Repo) -> PathBuf {
    repo.common_dir.join("bay").join("sessions")
}

fn bay_dir(repo: &Repo) -> PathBuf {
    repo.common_dir.join("bay")
}

/// The latest line of one session record.
fn last_line(path: &Path) -> Option<Value> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(std::io::SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    text.lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
}

/// The files `session` last wrote on this machine, from the tail of its local record: the
/// latest line whose `wrote` lists any file, as `{"wrote": [...], "wrote_at": ...}`; an empty
/// list when the tail names none; `null` fields when there is no record. A session id that is
/// not a plain file name is never read.
#[must_use]
pub fn last_wrote(repo: &Repo, session: &str) -> Value {
    let plain = !session.is_empty()
        && session != "."
        && session != ".."
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    let unknown = json!({"wrote": null, "wrote_at": null});
    if !plain {
        return unknown;
    }
    let path = sessions_dir(repo).join(format!("{session}.jsonl"));
    let Ok(mut file) = std::fs::File::open(path) else {
        return unknown;
    };
    let len = file.metadata().map_or(0, |meta| meta.len());
    let mut text = String::new();
    if file
        .seek(std::io::SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .and_then(|_| file.read_to_string(&mut text))
        .is_err()
    {
        return unknown;
    }
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| {
            line["wrote"]
                .as_array()
                .is_some_and(|wrote| !wrote.is_empty())
        })
        .map_or_else(
            || json!({"wrote": [], "wrote_at": null}),
            |line| json!({"wrote": line["wrote"], "wrote_at": line["at"]}),
        )
}

/// The Unix time now.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The Unix time of a summary's or row's `at`.
#[must_use]
pub fn unix_at(value: &Value) -> Option<u64> {
    value["at"].as_str().and_then(crate::clock::unix_of)
}

/// The rows a summary written at `now` carries for these latest record lines: each line's
/// [`ROW_FIELDS`], ended sessions and rows whose own time is `window` or more before `now`
/// left out, at most [`LIVE_MAX_ROWS`] (the newest), ordered by session.
#[must_use]
pub fn rows_of(lines: Vec<Value>, now: u64, window: u64) -> Vec<Value> {
    let mut rows: Vec<(u64, Value)> = lines
        .into_iter()
        .filter(|line| line["state"] != "ended")
        .filter_map(|line| {
            let at = unix_at(&line)?;
            (now.saturating_sub(at) < window).then_some((at, line))
        })
        .map(|(at, line)| {
            let row: Map<String, Value> = ROW_FIELDS
                .iter()
                .filter_map(|field| {
                    line.get(*field)
                        .filter(|value| !value.is_null())
                        .map(|value| ((*field).to_owned(), value.clone()))
                })
                .collect();
            (at, Value::Object(row))
        })
        .collect();
    rows.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    rows.truncate(LIVE_MAX_ROWS);
    let mut rows: Vec<Value> = rows.into_iter().map(|(_, row)| row).collect();
    rows.sort_by(|a, b| a["session"].as_str().cmp(&b["session"].as_str()));
    rows
}

/// The latest line of every session this repository records on this machine; `None` when it
/// records none.
#[must_use]
pub fn lines(repo: &Repo) -> Option<Vec<Value>> {
    let entries = std::fs::read_dir(sessions_dir(repo)).ok()?;
    Some(
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .filter_map(|path| last_line(&path))
            .collect(),
    )
}

/// The `doctor` block of `value` ([`DOCTOR_FIELDS`] alone); `None` when it carries none.
#[must_use]
pub fn doctor_block(value: &Value) -> Option<Value> {
    let block = value.as_object()?;
    Some(Value::Object(
        DOCTOR_FIELDS
            .iter()
            .filter_map(|field| {
                block
                    .get(*field)
                    .map(|held| ((*field).to_owned(), held.clone()))
            })
            .collect(),
    ))
}

/// G314: the `doctor` block the local live ref of `machine` carries: this machine's last
/// verdict as it last wrote it. `None` without the ref, or a summary that carries none.
#[must_use]
pub fn doctor_at(git: &Git, machine: &str) -> Option<Value> {
    let tip = git
        .run(&["rev-parse", "--verify", "-q", &live_ref(machine)])
        .ok()?;
    let summary = summary_at(git, tip.trim()).ok().flatten()?;
    doctor_block(&summary[DOCTOR_FIELD])
}

/// Writes this machine's summary commit and points the local live ref at it. `None` when this
/// repository has no session records here and no doctor verdict.
///
/// # Errors
///
/// A bad machine name, a missing agent key under a registry, or a git fault.
pub fn write(repo: &Repo) -> Result<Option<(String, String)>, String> {
    write_with(repo, None)
}

/// [`write`], carrying `doctor` as the summary's `doctor` block (G314); without one, the block
/// the local live ref already carries.
fn write_with(repo: &Repo, doctor: Option<Value>) -> Result<Option<(String, String)>, String> {
    let machine = machine(&repo.git)?;
    let doctor = doctor.or_else(|| doctor_at(&repo.git, &machine));
    let lines = lines(repo);
    if lines.is_none() && doctor.is_none() {
        return Ok(None);
    }
    let lines = lines.unwrap_or_default();
    let git = &repo.git;
    let key = if Registry::current(git)?.is_some_and(|registry| registry.signs_all()) {
        Some(Keys::load(git).agent.ok_or(
            "the repository has a key registry and this machine has no agent key (git config bay.agentKey)",
        )?)
    } else {
        None
    };
    let mut summary = json!({
        "schema": LIVE_SCHEMA,
        "machine": machine,
        "at": crate::clock::now_rfc3339(),
    });
    let actor = crate::change::current_actor(&repo.common_dir)?.name;
    if let Some(key) = &key {
        // The host checks both against the signature and binds the ref to them (§ 10a).
        summary["key"] = json!(signing::fingerprint(key)?);
        summary["signer"] = json!(actor);
    }
    summary["sessions"] = json!(rows_of(lines, unix_now(), repo.config.takeover().as_secs()));
    if let Some(doctor) = doctor {
        summary[DOCTOR_FIELD] = doctor;
    }
    let mut text = serde_json::to_string_pretty(&summary).map_err(|error| error.to_string())?;
    text.push('\n');
    let blob = git
        .run_with(
            &["hash-object", "-w", "--stdin"],
            &[],
            Some(text.as_bytes()),
        )
        .map_err(|error| error.message())?;
    let entry = format!("100644 blob {blob}\t{LIVE_FILE}\n");
    let tree = git
        .run_with(&["mktree"], &[], Some(entry.as_bytes()))
        .map_err(|error| error.message())?;
    let mut args: Vec<String> = Vec::new();
    if let Some(key) = &key {
        args.extend([
            "-c".to_owned(),
            "gpg.format=ssh".to_owned(),
            "-c".to_owned(),
            format!("user.signingkey={}", key.display()),
        ]);
    }
    args.extend(["commit-tree".to_owned(), tree]);
    args.push(if key.is_some() { "-S" } else { "--no-gpg-sign" }.to_owned());
    args.extend(["-m".to_owned(), format!("live {machine}")]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // Without a registry nothing is signed; the committer names the actor.
    let email = format!("{actor}@bay.invalid");
    let envs = [
        ("GIT_AUTHOR_NAME", actor.as_str()),
        ("GIT_AUTHOR_EMAIL", email.as_str()),
        ("GIT_COMMITTER_NAME", actor.as_str()),
        ("GIT_COMMITTER_EMAIL", email.as_str()),
    ];
    let commit = git
        .run_with(&args, &envs, None)
        .map_err(|error| error.message())?;
    let name = live_ref(&machine);
    git.run(&["update-ref", &name, &commit])
        .map_err(|error| error.message())?;
    Ok(Some((name, commit)))
}

fn push_once(repo: &Repo) -> Result<Value, String> {
    push_written(repo, write(repo)?)
}

/// G314, for `bay doctor`: writes this machine's summary carrying `doctor`, its verdict, and
/// pushes it, waiting for the push. Best effort: the outcome is kept as a live push's is, so a
/// failure shows in doctor's `live` row, and it never changes doctor's result.
pub fn publish_doctor(repo: &Repo, doctor: Value) {
    let outcome = write_with(repo, Some(doctor)).and_then(|written| push_written(repo, written));
    if std::fs::create_dir_all(bay_dir(repo)).is_ok() {
        keep_outcome(repo, &outcome);
    }
}

/// Pushes the summary commit [`write_with`] wrote, if it wrote one.
fn push_written(repo: &Repo, written: Option<(String, String)>) -> Result<Value, String> {
    let Some((name, commit)) = written else {
        return Ok(json!({"pushed": false, "why": "no sessions"}));
    };
    // The commit just written, never the local ref's name: a fetch of the remote's live refs
    // (`bay start`'s takeover watch) may move or prune that ref between the write and the push.
    repo.git
        .run(&["push", "-q", repo.remote(), &format!("+{commit}:{name}")])
        .map_err(|error| format!("{name} could not be pushed: {}", error.message()))?;
    Ok(json!({"ref": name, "commit": commit, "pushed": true}))
}

/// The pusher's lock file and how old it may be before it is taken as left by a dead pusher.
fn lock_of(repo: &Repo) -> (PathBuf, Duration) {
    let stale = repo.config.heartbeat() * (MAX_ROUNDS + 2) + Duration::from_secs(180);
    (bay_dir(repo).join("live-push.lock"), stale)
}

fn lock_is_held(lock: &Path, stale: Duration) -> bool {
    std::fs::metadata(lock)
        .and_then(|meta| meta.modified())
        .is_ok_and(|at| at.elapsed().unwrap_or_default() < stale)
}

fn acquire(lock: &Path, stale: Duration) -> bool {
    let create = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock)
            .is_ok()
    };
    if create() {
        return true;
    }
    if lock_is_held(lock, stale) {
        return false;
    }
    let _ = std::fs::remove_file(lock);
    create()
}

/// Whether a session record changed at or after `since`.
fn changed_since(repo: &Repo, since: SystemTime) -> bool {
    std::fs::read_dir(sessions_dir(repo)).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|at| at >= since)
        })
    })
}

/// The outcome of this repository's last live push, as the pusher kept it.
#[must_use]
pub fn last_push(common_dir: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(common_dir.join("bay").join("live-push.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn keep_outcome(repo: &Repo, outcome: &Result<Value, String>) {
    let record = match outcome {
        Ok(detail) => json!({"at": crate::clock::now_rfc3339(), "ok": true, "detail": detail}),
        Err(why) => json!({"at": crate::clock::now_rfc3339(), "ok": false, "why": why}),
    };
    let path = bay_dir(repo).join("live-push.json");
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, record.to_string()).is_ok() {
        let _ = crate::durable::rename(&tmp, &path);
    }
}

/// `bay live push`: writes the summary and pushes it, replacing the remote's live ref of this
/// machine (the hook accepts it only from the ref's bound key). One pusher runs at a time: it
/// pushes again, one heartbeat later, while the session records keep changing, up to
/// [`MAX_ROUNDS`] pushes.
#[must_use]
pub fn push(dir: &Path) -> BayResult {
    let repo = match Repo::open(dir) {
        Ok(repo) => repo,
        Err(why) => return BayResult::failed(why),
    };
    let (lock, stale) = lock_of(&repo);
    if std::fs::create_dir_all(bay_dir(&repo)).is_err() || !acquire(&lock, stale) {
        return BayResult::ok(json!({"pushed": false, "why": "another live push is running"}));
    }
    let mut rounds = 0;
    loop {
        let began = SystemTime::now();
        let outcome = push_once(&repo);
        keep_outcome(&repo, &outcome);
        rounds += 1;
        let detail = match outcome {
            Ok(detail) => detail,
            Err(why) => {
                let _ = std::fs::remove_file(&lock);
                return BayResult::failed(why);
            }
        };
        std::thread::sleep(repo.config.heartbeat());
        if rounds < MAX_ROUNDS && changed_since(&repo, began) {
            continue;
        }
        let _ = std::fs::remove_file(&lock);
        // An event between the last look and the release found the lock held and started no
        // pusher: look once more.
        if rounds < MAX_ROUNDS && changed_since(&repo, began) && acquire(&lock, stale) {
            continue;
        }
        return BayResult::ok(detail);
    }
}

/// After a hook event: starts one detached `bay live push` unless one is running. Never fails
/// the hook, never waits, and never touches the network itself.
pub fn after_event(dir: &Path, input: &str) {
    let payload: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    let cwd = crate::session::hook_cwd(&payload, dir);
    let Ok(repo) = Repo::open(&cwd) else {
        return;
    };
    if !sessions_dir(&repo).is_dir() {
        return;
    }
    let (lock, stale) = lock_of(&repo);
    if lock_is_held(&lock, stale) {
        return;
    }
    let Some(bay) = crate::update::own_binary() else {
        return;
    };
    let mut cmd = Command::new(bay);
    cmd.arg("-C")
        .arg(&cwd)
        .args(["--json", "live", "push"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::platform::detach(&mut cmd);
    let _ = cmd.spawn();
}

/// Whether `summary` holds a fresh row of `change` by `actor`: a row naming both whose own time
/// is less than `window` before the summary's time. Both times are the publishing machine's,
/// so the comparison does not depend on the reader's clock.
#[must_use]
pub fn names_change(summary: &Value, change: &str, actor: &str, window: u64) -> bool {
    let Some(written) = unix_at(summary) else {
        return false;
    };
    summary["sessions"].as_array().is_some_and(|rows| {
        rows.iter().any(|row| {
            row["change"] == change
                && row["actor"]["name"] == actor
                && unix_at(row).is_some_and(|at| written.saturating_sub(at) < window)
        })
    })
}

/// Whether live commit `commit` is a heartbeat of `change` by `actor` (§ 4): signed by that
/// actor's key in `agents` (without a registry, committed under that actor's name), with a
/// fresh row of the change ([`names_change`]).
///
/// # Errors
///
/// A git fault other than a failed verification.
pub fn heartbeat_of(
    git: &Git,
    commit: &str,
    change: &str,
    actor: &str,
    agents: Option<&Path>,
    window: u64,
) -> Result<bool, String> {
    if !summary_at(git, commit)?
        .is_some_and(|summary| names_change(&summary, change, actor, window))
    {
        return Ok(false);
    }
    Ok(signer_of(git, commit, agents)?.as_deref() == Some(actor))
}

/// Who signed live commit `commit`: the actor whose key in `agents` verifies it (without a
/// registry, the committer's name); `None` when it does not verify.
fn signer_of(git: &Git, commit: &str, agents: Option<&Path>) -> Result<Option<String>, String> {
    Ok(match agents {
        Some(file) => signing::verify_commit(git, commit, file).map_err(|error| error.message())?,
        None => git.run(&["log", "-1", "--format=%cn", commit]).ok(),
    })
}

/// Another live session of `me`'s actor name that holds a change: its session id and the age of
/// its heartbeat in seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveHolder {
    pub session: String,
    pub age_s: u64,
}

/// The other live session of `me`'s actor name whose heartbeat in live commit `commit` names
/// `change` (rulings-open-questions.md Q8, `DESIGN` § 4): a row of the change by that actor name
/// and of another session than `me`'s, not `ended`, whose own time is less than `window`
/// before `now` on this observer's clock, in a commit signed by that actor ([`heartbeat_of`]'s
/// signature rule). A heartbeat signed by anyone else holds nothing.
///
/// # Errors
///
/// A git fault other than a failed verification.
pub fn other_session_of(
    git: &Git,
    commit: &str,
    change: &str,
    me: &crate::change::Actor,
    agents: Option<&Path>,
    now: u64,
    window: u64,
) -> Result<Option<LiveHolder>, String> {
    let Some(summary) = summary_at(git, commit)? else {
        return Ok(None);
    };
    let holder = summary["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|row| {
            let session = row["session"].as_str()?;
            let age_s = now.saturating_sub(unix_at(row)?);
            (row["change"] == change
                && row["actor"]["name"] == me.name.as_str()
                && session != me.session
                && row["state"] != "ended"
                && age_s < window)
                .then(|| LiveHolder {
                    session: session.to_owned(),
                    age_s,
                })
        });
    let Some(holder) = holder else {
        return Ok(None);
    };
    Ok((signer_of(git, commit, agents)?.as_deref() == Some(me.name.as_str())).then_some(holder))
}

/// The `live.json` a live commit carries; `None` where its tree holds none or one that is not
/// UTF-8 JSON. G104b: a `live.json` the tree names whose blob cannot be read is an error that
/// names it, never `None`, so an unreadable heartbeat never reads as no heartbeat.
///
/// # Errors
///
/// The tree names `live.json` and its blob cannot be read, or a git fault.
pub fn summary_at(git: &Git, commit: &str) -> Result<Option<Value>, String> {
    match git.file_at(commit, LIVE_FILE) {
        Ok(text) => Ok(text.and_then(|text| serde_json::from_str(&text).ok())),
        Err(GitError::NotUtf8 { .. }) => Ok(None),
        Err(error) => Err(error.message()),
    }
}

/// One commit's `live.json` as [`summaries_at`] read it: `Ok(None)` where the commit holds
/// none, one larger than [`LIVE_MAX_BYTES`], or one that is not JSON; `Err` naming the file
/// where its tree names it and its blob cannot be read (G104b).
pub type SummaryRead = Result<Option<Value>, String>;

/// The `live.json` of each commit ([`SummaryRead`]). Two `git cat-file` calls whatever the
/// count: `--batch-check` sizes every file first, and `--batch` reads only the blobs within
/// the bound, as bytes, so a large or non-UTF-8 blob is never read and spoils no other summary.
/// G104b: `--batch-check` answers `missing` for a path the tree lacks and for a blob the store
/// lacks alike; each such answer is told apart by the tree alone, one `rev-parse` each, so an
/// unreadable summary is named, never read as no summary.
///
/// # Errors
///
/// A git fault.
pub fn summaries_at(git: &Git, commits: &[String]) -> Result<Vec<SummaryRead>, String> {
    files_at(git, commits, LIVE_FILE, LIVE_MAX_BYTES)
}

/// [`summaries_at`] for any one-file record: the JSON file `file` of each commit, read only
/// when it is at most `max` bytes (an executor's presence, G67, is read this way too).
///
/// # Errors
///
/// A git fault.
pub fn files_at(
    git: &Git,
    commits: &[String],
    file: &str,
    max: u64,
) -> Result<Vec<SummaryRead>, String> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = String::new();
    for commit in commits {
        input.push_str(commit);
        input.push(':');
        input.push_str(file);
        input.push('\n');
    }
    let checked = git
        .run_with(
            &[
                "cat-file",
                "--batch-check=%(objectname) %(objecttype) %(objectsize)",
            ],
            &[],
            Some(input.as_bytes()),
        )
        .map_err(|error| error.message())?;
    let lines: Vec<&str> = checked.lines().collect();
    if lines.len() != commits.len() {
        return Err(format!(
            "git cat-file --batch-check answered {} lines for {} commits",
            lines.len(),
            commits.len()
        ));
    }
    // Per commit, the blob to read (a blob within the bound), or why its file is unreadable.
    let mut wanted: Vec<Result<Option<String>, String>> = Vec::with_capacity(lines.len());
    for (commit, line) in commits.iter().zip(&lines) {
        wanted.push(if line.ends_with(" missing") {
            let spec = format!("{commit}:{file}");
            match git.run(&["rev-parse", "--verify", "-q", &spec]) {
                Ok(_) => Err(unreadable(&spec)),
                Err(GitError::Failed { .. }) => Ok(None),
                Err(error) => return Err(error.message()),
            }
        } else {
            Ok(small_blob(line, max))
        });
    }
    let mut blobs = String::new();
    for oid in wanted.iter().flatten().flatten() {
        blobs.push_str(oid);
        blobs.push('\n');
    }
    let bodies = if blobs.is_empty() {
        std::collections::BTreeMap::new()
    } else {
        let out = git
            .run_bytes(&["cat-file", "--batch"], &[], Some(blobs.as_bytes()))
            .map_err(|error| error.message())?;
        batch_bodies(&out)
    };
    Ok(wanted
        .into_iter()
        .zip(commits)
        .map(|(oid, commit)| -> SummaryRead {
            match oid? {
                None => Ok(None),
                // A blob sized a moment ago that `--batch` did not answer is unreadable too.
                Some(oid) => bodies
                    .get(&oid)
                    .cloned()
                    .ok_or_else(|| unreadable(&format!("{commit}:{file}"))),
            }
        })
        .collect())
}

/// G104b: why the file at `spec` (`<commit>:<path>`) reads as nothing: its tree names it, and
/// its blob cannot be read.
fn unreadable(spec: &str) -> String {
    format!("{spec} is named by its tree, but its blob cannot be read from this store or fetched")
}

/// The object id of a `--batch-check` line naming a blob of at most `max` bytes.
fn small_blob(line: &str, max: u64) -> Option<String> {
    let mut words = line.split(' ');
    let (Some(oid), Some("blob"), Some(size), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return None;
    };
    size.parse::<u64>()
        .is_ok_and(|size| size <= max)
        .then(|| oid.to_owned())
}

/// Each object of a `cat-file --batch` output by id, parsed as JSON (`None` when it is not).
fn batch_bodies(out: &[u8]) -> std::collections::BTreeMap<String, Option<Value>> {
    let mut bodies = std::collections::BTreeMap::new();
    let mut rest = out;
    while !rest.is_empty() {
        let end = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(rest.len());
        let header = String::from_utf8_lossy(&rest[..end]).into_owned();
        rest = rest.get(end + 1..).unwrap_or_default();
        let mut words = header.split(' ');
        let (Some(oid), Some(_), Some(Ok(size))) = (
            words.next(),
            words.next(),
            words.next().map(str::parse::<usize>),
        ) else {
            continue;
        };
        let body = rest.get(..size).unwrap_or(rest);
        rest = rest.get(size + 1..).unwrap_or_default();
        bodies.insert(oid.to_owned(), serde_json::from_slice::<Value>(body).ok());
    }
    bodies
}

/// The agent class file of the registry this repository holds, or `None` without one or under
/// `[policy] signing = "approvals"` (N-21), where live summaries are unsigned.
///
/// # Errors
///
/// A registry that does not load.
pub fn agents_file(git: &Git) -> Result<Option<PathBuf>, String> {
    match Registry::current(git)?.filter(Registry::signs_all) {
        Some(registry) => Ok(Some(registry.class_file(git, KeyClass::Agent)?)),
        None => Ok(None),
    }
}

/// Every live ref this repository holds, with its tip.
///
/// # Errors
///
/// A git fault.
pub fn tips(git: &Git) -> Result<std::collections::BTreeMap<String, String>, String> {
    let out = git
        .run(&[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            LIVE_PREFIX,
        ])
        .map_err(|error| error.message())?;
    Ok(out
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, tip)| (name.to_owned(), tip.to_owned()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{fold, names_change, rows_of};
    use serde_json::json;

    #[test]
    fn a_host_name_folds_to_an_operator_name() {
        assert_eq!(fold("Build-Box.example.org\n"), "build-box-example-org");
        assert_eq!(fold("..."), "machine");
        assert_eq!(fold(&"A".repeat(40)), "a".repeat(32));
        for name in ["Build-Box.example.org", "", "-x-", &"z".repeat(50)] {
            assert!(crate::names::operator_name(&fold(name)).is_ok(), "{name}");
        }
    }

    /// A record line written at Unix time `at`.
    fn line(session: &str, at: u64, state: &str) -> serde_json::Value {
        let secs = at % 60;
        json!({"schema": "bay.session/1", "at": format!("2026-10-19T00:00:{secs:02}Z"),
            "session": session, "actor": {"name": "a", "session": session}, "change": "c1",
            "repo": "/local/repo", "view": "/local/view", "event": "post_tool_batch",
            "heartbeat": true, "state": state})
    }

    #[test]
    fn a_dead_session_row_is_reaped_by_its_own_age() {
        let base = crate::clock::unix_of("2026-10-19T00:00:00Z").unwrap();
        let lines = vec![
            line("s-dead", base, "working"),
            line("s-live", base + 50, "working"),
            line("s-ended", base + 50, "ended"),
            json!({"session": "s-untimed", "state": "working"}),
        ];
        let rows = rows_of(lines, base + 55, 30);
        let sessions: Vec<_> = rows.iter().map(|row| row["session"].clone()).collect();
        assert_eq!(sessions, [json!("s-live")], "{rows:?}");
        // Local paths and the record schema stay local.
        assert!(rows[0].get("repo").is_none() && rows[0].get("view").is_none());
        assert!(rows[0].get("schema").is_none(), "{rows:?}");
        assert_eq!(rows[0]["change"], "c1");
    }

    #[test]
    fn an_idle_row_says_whether_its_view_holds_uncheckpointed_edits() {
        // Ruling R7: the row of a session that stopped over edits no checkpoint records says
        // so, and a stop that could not be judged says why.
        let base = crate::clock::unix_of("2026-10-19T00:00:00Z").unwrap();
        let mut idle = line("s-idle", base + 50, "idle");
        idle["uncheckpointed"] = json!(true);
        let mut unknown = line("s-unknown", base + 50, "idle");
        unknown["uncheckpointed"] = json!(null);
        unknown["uncheckpointed_unknown"] = json!("too_many_paths");
        let rows = rows_of(vec![idle, unknown], base + 55, 30);
        assert_eq!(rows[0]["uncheckpointed"], true, "{rows:?}");
        assert!(rows[1].get("uncheckpointed").is_none(), "{rows:?}");
        assert_eq!(
            rows[1]["uncheckpointed_unknown"], "too_many_paths",
            "{rows:?}"
        );
    }

    #[test]
    fn only_a_fresh_row_of_the_actor_names_its_change() {
        let summary = |row_at: &str, actor: &str| {
            json!({"schema": "bay.live/1", "machine": "m", "at": "2026-10-19T00:01:00Z",
                "sessions": [{"session": "s", "actor": {"name": actor}, "change": "c1",
                "at": row_at}]})
        };
        assert!(names_change(
            &summary("2026-10-19T00:00:45Z", "a"),
            "c1",
            "a",
            30
        ));
        // Its own heartbeat is older than the window: a sibling's summary does not revive it.
        assert!(!names_change(
            &summary("2026-10-19T00:00:30Z", "a"),
            "c1",
            "a",
            30
        ));
        assert!(!names_change(
            &summary("2026-10-19T00:00:45Z", "b"),
            "c1",
            "a",
            30
        ));
        assert!(!names_change(
            &summary("2026-10-19T00:00:45Z", "a"),
            "c2",
            "a",
            30
        ));
        assert!(!names_change(&summary("now", "a"), "c1", "a", 30));
    }
}

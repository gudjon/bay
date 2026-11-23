//! G873 (BACKLOG 253 b; a token-economy measurement, 2026-11-23: seats read `bay status` again
//! and again to learn whether their change merged, and each read costs a turn): the operations a
//! session left queued, so its idle wait (`bay hook talk-wait`, an asynchronous Stop hook) wakes
//! it once with each one's outcome.
//!
//! A command run by an agent's session (`BAY_SESSION`, or the session a hook named for the
//! agent's process, `crate::agent_pid`) that answers `queued` for an operation leaves a marker,
//! `<common dir>/bay/awaiting/<actor>/<op id>`, which holds the session. Any other answer of a
//! command for that operation removes it: the session read the outcome itself. A person's
//! command names no session and leaves none.
//!
//! The wait reads its actor's markers: its own session's, and those of a session no longer live
//! on this machine (BACKLOG 64 F2, as notices do), so a seat that restarts hears what the session
//! before it left. It fetches their chains in one round trip and gives one line for each
//! operation that ended: merged, once main holds its commit (G875: a record alone does not prove a
//! landing); no merge needed; or refused, failed or superseded, with its reason. A refusal that
//! clears without the agent (its retry is `by_itself` or `after_wait`) is no end: the runner
//! goes on with it. A line removes its marker, so each outcome wakes once. The marker of an
//! operation whose change has a later operation is removed untold: the later one has its own.
//!
//! A line is bay's own words, as a notice is (ruling R13): the change and operation ids, a reason
//! code bay knows, and a commit id; never text from the record.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::change::{Chain, OpState};
use crate::repo::Repo;
use crate::result::{BayResult, Decision, Reason};

/// What one poll of the wait heard besides talks.
#[derive(Debug, Default)]
pub struct Heard {
    /// The lines to wake the session with, one per operation that ended; empty for none.
    pub lines: String,
    /// One line per operation that still waits, told if the wait ends while it does.
    pub still: String,
    /// How many of the session's operations still wait.
    pub waiting: usize,
    /// The session's agent has ended: the wait ends too, and leaves its markers to the next.
    pub ended: bool,
}

fn actor_dir(common: &Path, actor: &str) -> PathBuf {
    common.join("bay").join("awaiting").join(actor)
}

/// The change of operation id `op`, when `op` has the form bay gives one: `<change>.<n>`.
fn change_of(op: &str) -> Option<&str> {
    let (change, n) = op.rsplit_once('.')?;
    (crate::ids::is_change_id(change)
        && !n.is_empty()
        && n.bytes().all(|byte| byte.is_ascii_digit()))
    .then_some(change)
}

fn seq(op: &str) -> Option<u64> {
    op.rsplit_once('.')?.1.parse().ok()
}

/// The agent's session that runs this command, if any: `BAY_SESSION`, else the session a hook
/// named for an ancestor of this process.
fn caller_session(common: &Path) -> Option<String> {
    std::env::var("BAY_SESSION")
        .ok()
        .or_else(|| crate::agent_pid::session_of_caller(common))
        .filter(|session| crate::session::SessionId::parse(session).is_some())
}

/// After a command in `dir`: a `queued` answer for an operation leaves its marker when an
/// agent's session ran the command; any other answer for it removes the marker. Best effort: a
/// marker that cannot be written leaves the session as it was before G873.
pub fn note(dir: &Path, result: &BayResult) {
    let Some(op) = result.op_id.as_deref().filter(|op| change_of(op).is_some()) else {
        return;
    };
    let git = crate::git::Git::new(dir);
    let Some(common) = git
        .common_dir_on_disk()
        .or_else(|| git.common_dir().ok().map(PathBuf::from))
    else {
        return;
    };
    let actor = crate::change::actor_name();
    if crate::names::operator_name(&actor).is_err() {
        return;
    }
    let dir = actor_dir(&common, &actor);
    let path = dir.join(op);
    if result.decision != Decision::Queued {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let Some(session) = caller_session(&common) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // Written beside its place, then renamed: a reader never sees half a marker. The part's name
    // is no operation id, so a wait never reads it.
    let part = dir.join(format!("{op}.part-{}", std::process::id()));
    if std::fs::write(&part, format!("{session}\n")).is_ok()
        && crate::durable::rename(&part, &path).is_err()
    {
        let _ = std::fs::remove_file(&part);
    }
}

/// One poll of the wait of `session`, an agent's session of `actor`: the lines of the marked
/// operations that ended, each marker removed as its line is made, and the ones that still wait.
#[must_use]
pub fn hear(repo: &Repo, actor: &str, session: Option<&str>) -> Heard {
    let Ok(entries) = std::fs::read_dir(actor_dir(&repo.common_dir, actor)) else {
        return Heard::default();
    };
    let found: Vec<(String, PathBuf, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let op = entry.file_name().to_str()?.to_owned();
            change_of(&op)?;
            let holder = std::fs::read_to_string(entry.path()).ok()?;
            Some((op, entry.path(), holder.trim().to_owned()))
        })
        .collect();
    if found.is_empty() {
        return Heard::default();
    }
    // F2: this session's own operations, and those of a session no longer live.
    let live = crate::notice::live_sessions(repo);
    let mut marked: Vec<(String, PathBuf)> = found
        .into_iter()
        .filter(|(_, _, holder)| session == Some(holder.as_str()) || !live.contains(holder))
        .map(|(op, path, _)| (op, path))
        .collect();
    if marked.is_empty() {
        return Heard::default();
    }
    marked.sort();
    let mut changes: Vec<String> = marked
        .iter()
        .filter_map(|(op, _)| change_of(op).map(str::to_owned))
        .collect();
    changes.dedup();
    // One round trip for every marked chain. A chain the remote cannot answer is read as this
    // store holds it, and its operation waits on.
    let _ = repo.remote_chains(&changes);
    let chain = Chain::new(&repo.git);
    let mut heard = Heard::default();
    for (op, path) in marked {
        let change = change_of(&op).unwrap_or_default();
        match ended(repo, &chain, change, &op) {
            End::Waits(state) => {
                heard.waiting += 1;
                let _ = writeln!(
                    heard.still,
                    "bay: change {change}: {op} is still {state} at the end of this wait; nothing to do, the next wait goes on"
                );
            }
            End::Passed => {
                let _ = std::fs::remove_file(&path);
            }
            // Only the wait that removed the marker tells the line.
            End::Told(told) => {
                if std::fs::remove_file(&path).is_ok() {
                    heard.lines.push_str(&told);
                }
            }
        }
    }
    heard
}

/// Where one marked operation stands.
enum End {
    /// Under way, or merged where main does not hold it yet: in this state.
    Waits(&'static str),
    /// A later operation of its change carries it: removed untold.
    Passed,
    /// Ended: the line to tell.
    Told(String),
}

fn ended(repo: &Repo, chain: &Chain, change: &str, op: &str) -> End {
    let Ok((_, record)) = chain.read(change) else {
        return End::Waits("unread");
    };
    let Some(operation) = record
        .operations
        .iter()
        .find(|operation| operation.op_id == op)
    else {
        return End::Waits("unread");
    };
    let later = record
        .operations
        .iter()
        .any(|other| seq(&other.op_id) > seq(op));
    let outcome = operation.outcome.as_deref();
    match operation.state {
        OpState::Published => match operation.attempts.last() {
            Some(attempt) if crate::publish::main_holds(repo, &attempt.commit) => {
                End::Told(merged_line(change, op, &attempt.commit))
            }
            // G875: not merged until main holds it.
            _ => End::Waits("published"),
        },
        _ if later => End::Passed,
        OpState::NoEffect => End::Told(format!(
            "bay: change {change}: {op} needed no merge, main already holds its tree; nothing more to do\n"
        )),
        OpState::Blocked
            if outcome.and_then(Reason::from_code).is_some_and(|reason| {
                matches!(reason.retry_when(), "by_itself" | "after_wait")
            }) =>
        {
            End::Waits("blocked")
        }
        OpState::Blocked => End::Told(crate::notice::op_line(change, "refused", outcome, op)),
        OpState::Failed => End::Told(crate::notice::op_line(change, "failed", outcome, op)),
        OpState::Superseded => End::Told(crate::notice::op_line(change, "superseded", outcome, op)),
        OpState::Submitted => End::Waits("submitted"),
        OpState::Admitted => End::Waits("admitted"),
        OpState::Running => End::Waits("running"),
        OpState::Checked => End::Waits("checked"),
    }
}

/// The line of a merged operation: its commit's first twelve hex digits, or a fixed word for a
/// commit id of another form.
fn merged_line(change: &str, op: &str, commit: &str) -> String {
    let shown = if commit.len() >= 12 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        &commit[..12]
    } else {
        "a commit"
    };
    format!("bay: change {change}: merged {op} as {shown}; nothing more to do\n")
}

#[cfg(test)]
mod tests {
    use super::{change_of, seq};

    #[test]
    fn an_operation_id_names_its_change() {
        assert_eq!(change_of("abcd2345.1"), Some("abcd2345"));
        assert_eq!(change_of("abcd2345.12"), Some("abcd2345"));
        assert_eq!(change_of("abcd2345.1.part-77"), None);
        assert_eq!(change_of("abcd2345."), None);
        assert_eq!(change_of("../x.1"), None);
        assert!(seq("abcd2345.2") > seq("abcd2345.1"));
    }
}

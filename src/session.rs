//! The agent hook's session events (`DESIGN` § 11, § 11a).
//!
//! Built here: `session-start` (ruling 8a moves it into increment 1). It prints the compact
//! state of the change whose view the session runs in, every time, also when nothing changed,
//! and the open refusal an outcome left on it, so a result reached after its agent left
//! (`INTERFACES` § 2, "Outcomes that arrive after the agent left") reaches the next session.
//! Claude Code adds a `SessionStart` hook's stdout to the session's context; the hook never
//! blocks the session, so it always exits 0. It reads the local cache only (`status::cached`):
//! no fetch, no receipt push, no journal write, so a slow remote never delays a session and
//! sessions that start together never race; the trunk line says it is the local cache, and
//! `bay status` is what synchronizes (`DESIGN` § 11). Overlapping changes are a later item.
//! Every probed hook event in a repository but pre-tool also appends to the session's work
//! record (`record`), with the files each step wrote in a view.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agent_hook::{Event, Payload};
use crate::change::{ChangeRecord, ChangeState, OpState};
use crate::repo::Repo;
use crate::result::Reason;

/// The hook's working directory: the payload's `cwd`, else the directory bay runs in.
#[must_use]
pub fn hook_cwd(payload: &Value, dir: &Path) -> PathBuf {
    payload["cwd"]
        .as_str()
        .map_or_else(|| dir.to_path_buf(), PathBuf::from)
}

/// A session id that names a file in the sessions directory and nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionId(String);

impl SessionId {
    #[must_use]
    pub fn parse(session: &str) -> Option<Self> {
        let safe = !session.is_empty()
            && session.len() <= 128
            && !session.starts_with('.')
            && session
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        safe.then(|| Self(session.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Ruling R2: where the product hands `SessionStart` a `CLAUDE_ENV_FILE` (Claude Code sources it
/// before each shell command of the session), bay appends `BAY_SESSION` and `BAY_ACTOR` to it,
/// so the session's own bay commands act as this session, and marks the link `bound` in the
/// session's state. Without one the link is `unbound`: never guessed. An actor name with a
/// control character is not written.
fn bind_env(state_dir: &Path, session: &SessionId) {
    let Some(file) = std::env::var_os("CLAUDE_ENV_FILE").filter(|file| !file.is_empty()) else {
        return;
    };
    let actor = crate::change::actor_name();
    if actor.is_empty() || actor.chars().any(char::is_control) {
        return;
    }
    let quote = |value: &str| format!("'{}'", value.replace('\'', r"'\''"));
    let text = format!(
        "export BAY_SESSION={}\nexport BAY_ACTOR={}\n",
        quote(session.as_str()),
        quote(&actor)
    );
    let appended = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .and_then(|mut file| std::io::Write::write_all(&mut file, text.as_bytes()));
    if appended.is_ok() && std::fs::create_dir_all(state_dir).is_ok() {
        let _ = std::fs::write(state_dir.join("link"), "CLAUDE_ENV_FILE\n");
    }
}

/// Appends one `bay.session/1` line for this hook event to
/// `<git-common-dir>/bay/sessions/<session-id>.jsonl` (§ 11a), when the session runs in a
/// repository. `PreToolUse` appends nothing: it is the blocking path (`onFailure: "block"`),
/// § 11a gives it only the refusal, and the same step's `PostToolBatch` records it.
///
/// A view belongs to one change (ruling 10): every session whose hook fires in the view works on
/// that view's change and is recorded by its own session id under the change's actor, a
/// subagent with its `agent_id`, a teammate with its own `session_id`. At the post-tool and stop
/// events in a view the line lists the files the step wrote (`wrote`), or why they are unknown
/// (`wrote_unknown`), from the view's own Git status (§ 11a, `crate::writes`). The record is a
/// disposable work record; a hook never fails on it. Files read and the machine's summary ref
/// are later items.
pub fn record(payload: &Payload) {
    let Some(session) = &payload.session else {
        return;
    };
    let cwd = &payload.cwd;
    let Ok(repo) = Repo::open(cwd) else {
        return;
    };
    // In a view, the session works on the view's change, under the change's actor. Elsewhere in
    // the repository it is still a tracked session (§ 11a reserves "untracked" for hooks
    // disabled): no change, no view, and the actor's own name with this session.
    let (actor, change, view) = match repo.view_change(cwd) {
        Some(id) => {
            let Ok((_, change)) = crate::change::Chain::new(&repo.git).read(&id) else {
                return;
            };
            let view = repo.view_path(&id);
            (change.actor, Some(id), Some(view))
        }
        None => (
            crate::change::Actor {
                name: crate::change::actor_name(),
                session: session.as_str().to_owned(),
            },
            None,
            None,
        ),
    };
    let dir = repo.common_dir.join("bay").join("sessions");
    let path = dir.join(format!("{}.jsonl", session.as_str()));
    let state_dir = repo
        .common_dir
        .join("bay")
        .join("session-state")
        .join(session.as_str());
    let event = payload.event;
    if event == Event::SessionStart && payload.agent.is_none() {
        bind_env(&state_dir, session);
    }
    let actor_link = if state_dir.join("link").is_file() {
        "bound"
    } else {
        "unbound"
    };
    let mut line = serde_json::json!({
        "schema": "bay.session/1",
        "at": crate::clock::now_rfc3339(),
        "session": session.as_str(),
        "actor": actor,
        "actor_link": actor_link,
        "repo": repo.work_root.display().to_string(),
        "change": change,
        "view": view.as_ref().map(|view| view.display().to_string()),
        "event": event.record_name(),
        "heartbeat": true,
    });
    if let Some(agent) = &payload.agent {
        line["agent"] = Value::from(agent.as_str());
    }
    if let Some(state) = event.state_after() {
        line["state"] = Value::from(state);
    }
    let state = crate::writes::state_path(&state_dir, payload.agent.as_deref());
    if let (Some(view), Some(step), Some(state)) = (&view, event.writes(), &state) {
        match crate::writes::step(view, state, step) {
            Some(crate::writes::Wrote::Paths(paths)) => line["wrote"] = serde_json::json!(paths),
            Some(crate::writes::Wrote::Unknown(why)) => line["wrote_unknown"] = Value::from(why),
            None => {}
        }
    }
    let Ok(mut text) = serde_json::to_string(&line) else {
        return;
    };
    text.push('\n');
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // One write of one whole line to a file opened for append: concurrent hooks never
    // interleave within a line.
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = std::io::Write::write_all(&mut file, text.as_bytes());
    }
    // The session is over: its kept ids and its link go, so this state never outgrows the live
    // sessions. A resumed session starts a new baseline.
    if event == Event::SessionEnd {
        let _ = std::fs::remove_dir_all(&state_dir);
    }
}

/// The text `bay hook session-start` prints for `input` (the product's payload): the compact
/// state of the view's change when the session runs in a view, then each other local change
/// of the same actor that holds an open refusal (`INTERFACES` § 2: a refusal reached after the
/// agent left goes "to a later session of the same actor at `SessionStart`"), wherever in the
/// repository that session starts. Empty when there is neither.
#[must_use]
pub fn session_start(dir: &Path, input: &str) -> String {
    let payload: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    let cwd = hook_cwd(&payload, dir);
    let Ok(repo) = Repo::open(&cwd) else {
        return String::new();
    };
    let view = repo.view_change(&cwd);
    let actor = crate::change::actor_name();
    // A local read: a hook never fetches, pushes or writes view journals (`DESIGN` § 11, hook
    // latency is a measured interface). `bay status` is what synchronizes.
    let select = |id: &str, record: &ChangeRecord| {
        view.as_deref() == Some(id) || (record.actor.name == actor && may_be_refused(record))
    };
    let result =
        crate::status::cached(&repo, &select).unwrap_or_else(crate::result::BayResult::failed);
    let json = result
        .to_json()
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null);
    let mut out = view
        .as_deref()
        .map(|id| compact(id, &json))
        .unwrap_or_default();
    for change in json["detail"]["changes"].as_array().into_iter().flatten() {
        let Some(id) = change["change"].as_str() else {
            continue;
        };
        if view.as_deref() != Some(id) && open_refusal(change).is_some() {
            out.push_str(&compact(id, &json));
        }
    }
    out
}

/// A change whose chain alone allows an open refusal: not closed, latest operation refused.
/// Whether it landed is read from trunk afterwards.
fn may_be_refused(record: &ChangeRecord) -> bool {
    !matches!(
        record.state,
        ChangeState::Landed | ChangeState::Abandoned | ChangeState::Superseded
    ) && record
        .operations
        .last()
        .is_some_and(|op| matches!(op.state, OpState::Blocked | OpState::Failed))
}

/// The open refusal of one change in a `bay status` result: its latest operation ended blocked
/// or failed with a reason code, and the change has not landed or closed since. A newer
/// submission replaces it as the latest operation.
fn open_refusal(change: &Value) -> Option<String> {
    let state = change["state"].as_str().unwrap_or("unknown");
    let operation = &change["operation"];
    let op_id = operation["op_id"].as_str()?;
    let op_state = operation["state"].as_str().unwrap_or("unknown");
    if matches!(state, "landed" | "abandoned" | "superseded")
        || !matches!(op_state, "blocked" | "failed")
    {
        return None;
    }
    let reason = operation["outcome"].as_str().and_then(Reason::from_code)?;
    let allowed = if reason.allowed().is_empty() {
        "none needed".to_owned()
    } else {
        reason.allowed().join(", ")
    };
    Some(format!(
        "bay: refused {} on {op_id}: allowed {allowed}; retry when {}\n",
        reason.code(),
        reason.retry_when()
    ))
}

/// The compact state of one change from its `bay status` result: one line for the change, one
/// for its latest operation, one for an open refusal.
fn compact(id: &str, status: &Value) -> String {
    let Some(change) = status["detail"]["changes"]
        .as_array()
        .and_then(|changes| changes.iter().find(|change| change["change"] == id))
    else {
        let why = status["detail"].as_str().unwrap_or("no status");
        return format!("bay: change {id}: state unavailable ({why})\n");
    };
    let state = change["state"].as_str().unwrap_or("unknown");
    let mut out = format!(
        "bay: change {id} (intent {}): {state}",
        change["intent"].as_str().unwrap_or("?")
    );
    if let Some(trunk) = status["freshness"]["trunk"].as_str() {
        let _ = write!(out, "; trunk {}", &trunk[..trunk.len().min(12)]);
        match status["freshness"]["fetched_at"].as_str() {
            Some(crate::status::CACHED) => out.push_str(" (local cache; bay status fetches)"),
            Some(at) if at.starts_with("not fetched") => {
                out.push_str(" (not fetched: remote unreachable)");
            }
            _ => {}
        }
    }
    out.push('\n');
    let operation = &change["operation"];
    if let Some(op_id) = operation["op_id"].as_str() {
        let op_state = operation["state"].as_str().unwrap_or("unknown");
        let _ = write!(out, "bay: operation {op_id}: {op_state}");
        if let Some(outcome) = operation["outcome"].as_str() {
            let _ = write!(out, ", outcome {outcome}");
        }
        out.push('\n');
        if let Some(refusal) = open_refusal(change) {
            out.push_str(&refusal);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::compact;

    #[test]
    fn a_blocked_operation_is_an_open_refusal() {
        let status = json!({"detail": {"changes": [{
            "change": "abcd2345", "intent": "CARD-1", "state": "open",
            "operation": {"op_id": "abcd2345.1", "state": "blocked", "outcome": "CHECK_FAILED"},
        }]}, "freshness": {"trunk": "0123456789abcdef", "fetched_at": "2026-10-18T00:00:00Z"}});
        let text = compact("abcd2345", &status);
        assert!(
            text.contains("bay: change abcd2345 (intent CARD-1): open; trunk 0123456789ab\n"),
            "{text}"
        );
        assert!(
            text.contains("bay: refused CHECK_FAILED on abcd2345.1: allowed repair; retry when candidate_changed\n"),
            "{text}"
        );
    }

    #[test]
    fn a_landed_change_has_no_open_refusal() {
        let status = json!({"detail": {"changes": [{
            "change": "abcd2345", "intent": "CARD-1", "state": "landed",
            "operation": {"op_id": "abcd2345.1", "state": "blocked", "outcome": "CHECK_FAILED"},
        }]}});
        assert!(!compact("abcd2345", &status).contains("refused"));
    }

    #[test]
    fn a_failed_status_still_names_the_change() {
        let status = json!({"decision": "failed", "detail": "no such change"});
        assert_eq!(
            compact("abcd2345", &status),
            "bay: change abcd2345: state unavailable (no such change)\n"
        );
    }
}

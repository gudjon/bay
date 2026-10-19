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
//! `bay status` is what synchronizes (`DESIGN` § 11). Overlapping changes and `watchPaths` are later
//! items. Every hook event in a view also appends to the session's work record (`record`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;

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

/// The session's state after `event`, where § 11a's wiring names one.
fn state_after(event: &str) -> Option<&'static str> {
    match event {
        "user_prompt_submit" | "post_tool_batch" => Some("working"),
        "notification" => Some("waiting"),
        "stop" => Some("idle"),
        "session_end" => Some("ended"),
        _ => None,
    }
}

/// `PostToolBatch` → `post_tool_batch`.
fn snake_case(name: &str) -> String {
    let mut out = String::new();
    for (index, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else if c == '-' {
            out.push('_');
        } else {
            out.push(c);
        }
    }
    out
}

/// A session id that names a file in the sessions directory and nothing else.
fn file_safe(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 128
        && !session.starts_with('.')
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Appends one `bay.session/1` line for this hook event to
/// `<git-common-dir>/bay/sessions/<session-id>.jsonl` (§ 11a), when the session runs in a
/// repository. `PreToolUse` appends nothing: it is the blocking path (`onFailure: "block"`),
/// § 11a gives it only the refusal, and the same step's `PostToolBatch` records it.
///
/// A view belongs to one change (ruling 10): every session whose hook fires in the view works on
/// that view's change and is recorded by its own session id under the change's actor, a
/// subagent with its `agent_id`, a teammate with its own `session_id`. The record is a
/// disposable work record; a hook never fails on it. Files read and written (the view's Git
/// status diff) and the machine's summary ref are later items.
pub fn record(dir: &Path, event: &str, input: &str) {
    if event == "pre-tool" {
        return;
    }
    let Ok(payload) = serde_json::from_str::<Value>(input) else {
        return;
    };
    let Some(session) = payload["session_id"].as_str().filter(|id| file_safe(id)) else {
        return;
    };
    let cwd = hook_cwd(&payload, dir);
    let Ok(repo) = Repo::open(&cwd) else {
        return;
    };
    // In a view, the session works on the view's change, under the change's actor. Elsewhere in
    // the repository it is still a tracked session (§ 11a reserves "untracked" for hooks
    // disabled): no change, no view, and the actor's own name with this session.
    let (actor, change, view) = match repo.view_change(&cwd) {
        Some(id) => {
            let Ok((_, change)) = crate::change::Chain::new(&repo.git).read(&id) else {
                return;
            };
            let view = repo.view_path(&id).display().to_string();
            (change.actor, Value::from(id), Value::from(view))
        }
        None => (
            crate::change::Actor {
                name: crate::change::actor_name(),
                session: session.to_owned(),
            },
            Value::Null,
            Value::Null,
        ),
    };
    let event = payload["hook_event_name"]
        .as_str()
        .map_or_else(|| snake_case(event), snake_case);
    let mut line = serde_json::json!({
        "schema": "bay.session/1",
        "at": crate::clock::now_rfc3339(),
        "session": session,
        "actor": actor,
        "repo": repo.work_root.display().to_string(),
        "change": change,
        "view": view,
        "event": event,
        "heartbeat": true,
    });
    if let Some(agent) = payload["agent_id"].as_str() {
        line["agent"] = Value::from(agent);
    }
    if let Some(state) = state_after(&event) {
        line["state"] = Value::from(state);
    }
    let Ok(mut text) = serde_json::to_string(&line) else {
        return;
    };
    text.push('\n');
    let dir = repo.common_dir.join("bay").join("sessions");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // One write of one whole line to a file opened for append: concurrent hooks never
    // interleave within a line.
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{session}.jsonl")))
    {
        let _ = std::io::Write::write_all(&mut file, text.as_bytes());
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

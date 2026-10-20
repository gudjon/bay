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
//! record (`record`), with the files each step wrote in a view, and gives the session's agent
//! the notices of its change it was not yet told (`crate::notice`).

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
/// repository. A silent `PreToolUse` appends nothing: it is the blocking path
/// (`onFailure: "block"`), § 11a gives it only the refusal, and the same step's
/// `PostToolBatch` records it; a refusal, a shell deny or a notice it gives is recorded
/// ([`record_with`]).
///
/// A view belongs to one change (ruling 10): every session whose hook fires in the view works on
/// that view's change and is recorded by its own session id under the change's actor, a
/// subagent with its `agent_id`, a teammate with its own `session_id`. At the post-tool and stop
/// events in a view the line lists the files the step wrote (`wrote`), or why they are unknown
/// (`wrote_unknown`), from the view's own Git status (§ 11a, `crate::writes`). The record is a
/// disposable work record; a hook never fails on it. Files read and the machine's summary ref
/// are later items.
///
/// The session's `Stop` in a view also records whether the view holds uncheckpointed edits
/// (`uncheckpointed`, `DESIGN` § 4), and ruling R7 refuses that stop once: with
/// `stop_hook_active` false the line stays `working` with `stop_refused`, and the refusal's
/// reason is returned for the hook to give the agent. Once is per set of edits, and bay keeps
/// it itself (the recordings show `stop_hook_active` only as false): the next stop over the same
/// edits is allowed, and the session is idle with its uncheckpointed edits. A stop with
/// `stop_hook_active` true or absent, and edits that cannot be read (`uncheckpointed_unknown`),
/// are never refused.
///
/// At `session-start` the session's delivered-notice cursor begins, and at `post-tool` and
/// `user-prompt-submit` the session's own agent is given the notices of its view's change and
/// of its actor's other changes it was not yet told (`crate::notice`, ruling R6); the line names them.
///
/// Every line names the product's permission mode (`permission_mode`, G153): the payload's, else
/// the last one the session named, and when the session started (`started`, G166).
///
/// G165: a session killed without a stop loses at most one interval. In a view, the session's
/// own `SessionEnd`, and any event `[hooks] checkpoint_idle_s` (default 600) after its last
/// checkpoint, start one detached checkpoint of the view, pushed like any checkpoint
/// (`crate::start::checkpoint`); the line names it (`checkpoint`).
#[must_use]
pub fn record(payload: &Payload) -> Recorded {
    record_with(payload, &serde_json::Map::new())
}

/// What recording one hook event gives the hook to return.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recorded {
    /// Ruling R7's reason, when the event is a stop it refuses.
    pub stop_refused: Option<String>,
    /// The notice lines for the agent, each ending in a newline; empty for none.
    pub notice: String,
}

/// [`record`], with `extra` fields on the line: a `pre-tool` refusal's `blocked(<reason>)`
/// state (a stale write, a shell command on the deny list by its shape, a payload bay cannot
/// read), or the `COVERAGE_UNKNOWN` notice it allowed a write with (ruling R5), each with its
/// AGENT-HOOKS-0.1 `verdict` (ruling R-H2). The silent `pre-tool` path appends nothing. It
/// returns what [`record`] does.
#[must_use]
pub fn record_with(payload: &Payload, extra: &serde_json::Map<String, Value>) -> Recorded {
    record_line(payload, extra).unwrap_or_default()
}

fn record_line(payload: &Payload, extra: &serde_json::Map<String, Value>) -> Option<Recorded> {
    let session = payload.session.as_ref()?;
    let cwd = &payload.cwd;
    let repo = Repo::open(cwd).ok()?;
    // In a view, the session works on the view's change, under the change's actor. Elsewhere in
    // the repository it is still a tracked session (§ 11a reserves "untracked" for hooks
    // disabled): no change, no view, and the actor's own name with this session. The row says
    // where the actor's name came from (G152): the change's record in a view, else this process.
    let mut at_stop = None;
    let (actor, actor_source, change, view) = match repo.view_change(cwd) {
        Some(id) => {
            let (head, change) = crate::change::Chain::new(&repo.git).read(&id).ok()?;
            if payload.event == Event::Stop && payload.agent.is_none() {
                at_stop = Some(AtStop {
                    head,
                    cut: change.retention_cut,
                });
            }
            let view = repo.view_path(&id);
            (change.actor, change.actor_source, Some(id), Some(view))
        }
        None => (
            crate::change::Actor {
                name: crate::change::actor_name(),
                session: session.as_str().to_owned(),
            },
            Some(crate::change::actor_source()),
            None,
            None,
        ),
    };
    let dir = repo.common_dir.join("bay").join("sessions");
    let path = dir.join(format!("{}.jsonl", session.as_str()));
    let state_dir = state_dir(&repo, session);
    let event = payload.event;
    if event == Event::SessionStart && payload.agent.is_none() {
        bind_env(&state_dir, session);
        crate::notice::begin(&repo, &state_dir);
    }
    let actor_link = actor_link(&state_dir);
    let mut line = serde_json::json!({
        "schema": "bay.session/1",
        "at": crate::clock::now_rfc3339(),
        "session": session.as_str(),
        "actor": actor,
        "actor_source": actor_source,
        "actor_link": actor_link,
        "repo": repo.work_root.display().to_string(),
        "change": change,
        "view": view.as_ref().map(|view| view.display().to_string()),
        "event": event.record_name(),
        "heartbeat": true,
    });
    payload_fields(&mut line, payload, &state_dir);
    if let Some(state) = event.state_after() {
        line["state"] = Value::from(state);
    }
    for (key, value) in extra {
        line[key] = value.clone();
    }
    let scope = crate::notice::Scope::new(change.as_deref(), &actor.name);
    let notice = notices(&repo, scope, payload, &state_dir, &mut line);
    let state = crate::writes::state_path(&state_dir, payload.agent.as_deref());
    let stop_refused = match (&view, event.writes(), &state, &change) {
        (Some(view), Some(step), Some(state), Some(id)) => step_fields(
            &mut line,
            payload,
            (view, state, &state_dir),
            step,
            at_stop.as_ref(),
            id,
        ),
        _ => None,
    };
    settle(
        &repo,
        payload,
        view.as_deref(),
        &mut line,
        &state_dir,
        stop_refused.is_some(),
    );
    let recorded = Recorded {
        stop_refused,
        notice,
    };
    let Ok(mut text) = serde_json::to_string(&line) else {
        return Some(recorded);
    };
    text.push('\n');
    if std::fs::create_dir_all(&dir).is_err() {
        return Some(recorded);
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
    Some(recorded)
}

/// The hook's own state of `session`: `<git-common-dir>/bay/session-state/<session>`.
fn state_dir(repo: &Repo, session: &SessionId) -> PathBuf {
    repo.common_dir
        .join("bay")
        .join("session-state")
        .join(session.as_str())
}

/// The fields of the line that come from the payload: a subagent's `agent`, and the product's
/// `permission_mode` (G153); and when the session started (`started`, G166).
fn payload_fields(line: &mut Value, payload: &Payload, state_dir: &Path) {
    if let Some(agent) = &payload.agent {
        line["agent"] = Value::from(agent.as_str());
    }
    if let Some(mode) = permission_mode(state_dir, payload) {
        line["permission_mode"] = Value::from(mode);
    }
    if let Some(started) = started(state_dir, &line["at"]) {
        line["started"] = Value::from(started);
    }
}

/// The file in a session's state directory that holds the time of its first recorded event.
const STARTED_FILE: &str = "started";

/// When the session started (G166): the time of the first event the hook recorded for it, kept
/// in its state, so `bay status --fleet` shows each session's age. `at` is this line's time; it
/// is kept when the session has none yet. A resumed session starts again, as its state does.
fn started(state_dir: &Path, at: &Value) -> Option<String> {
    let path = state_dir.join(STARTED_FILE);
    let mut kept = String::new();
    if let Ok(file) = std::fs::File::open(&path) {
        let _ = std::io::Read::read_to_string(&mut std::io::Read::take(file, 64), &mut kept);
        if crate::clock::unix_of(&kept).is_some() {
            return Some(kept);
        }
    }
    let at = at.as_str()?;
    if std::fs::create_dir_all(state_dir).is_ok() {
        let _ = std::fs::write(&path, at);
    }
    Some(at.to_owned())
}

/// The file in a session's state directory that holds the last permission mode it named.
const MODE_FILE: &str = "permission-mode";

/// The agent product's permission mode for this line (G153): the payload's, kept in the
/// session's state, else the last one the session named, since not every payload names one
/// (the recorded `SessionStart` does not) and the latest line is what the fleet map shows.
fn permission_mode(state_dir: &Path, payload: &Payload) -> Option<String> {
    let path = state_dir.join(MODE_FILE);
    if let Some(mode) = &payload.permission_mode {
        let known = std::fs::read_to_string(&path).is_ok_and(|kept| kept == *mode);
        if !known && std::fs::create_dir_all(state_dir).is_ok() {
            let _ = std::fs::write(&path, mode);
        }
        return Some(mode.clone());
    }
    let mut kept = String::new();
    std::io::Read::read_to_string(
        &mut std::io::Read::take(std::fs::File::open(&path).ok()?, 32),
        &mut kept,
    )
    .ok()?;
    (!kept.is_empty() && kept.bytes().all(|byte| byte.is_ascii_alphabetic())).then_some(kept)
}

/// Ruling R2's link of the session's bay commands to the session: `bound` once `SessionStart`
/// wrote them into the product's environment file.
fn actor_link(state_dir: &Path) -> &'static str {
    if state_dir.join("link").is_file() {
        "bound"
    } else {
        "unbound"
    }
}

/// What the line's event settles beyond the line: the session's claims ([`claims`]) and, in a
/// view, its checkpoint ([`checkpoint_due`]), which the line names (`checkpoint`).
fn settle(
    repo: &Repo,
    payload: &Payload,
    view: Option<&Path>,
    line: &mut Value,
    state_dir: &Path,
    stop_refused: bool,
) {
    claims(repo, payload, view, line, state_dir, stop_refused);
    let Some(id) = line["change"].as_str().map(str::to_owned) else {
        return;
    };
    if let Some(why) = checkpoint_due(repo, payload, state_dir)
        && start_checkpoint(&payload.cwd, &id)
    {
        line["checkpoint"] = Value::from(why);
    }
}

/// The file in a session's state directory that holds the Unix time of its last checkpoint.
const CHECKPOINT_FILE: &str = "checkpointed-at";

/// Whether this event checkpoints the session's view (G165), and why: the session's own end
/// (`session_end`), or an event `[hooks] checkpoint_idle_s` or more after the session's last
/// checkpoint (`idle`), so a session killed without a stop loses at most one interval. Ruling
/// R7's stop refusal covers only a clean stop. The interval runs from the session's first event,
/// and a clock that went back restarts it.
fn checkpoint_due(repo: &Repo, payload: &Payload, state_dir: &Path) -> Option<&'static str> {
    if payload.event == Event::SessionEnd {
        return payload.agent.is_none().then_some("session_end");
    }
    let path = state_dir.join(CHECKPOINT_FILE);
    let now = crate::live::unix_now();
    let last = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok());
    let due = last
        .is_some_and(|last| now >= last && now - last >= repo.config.checkpoint_idle().as_secs());
    if (due || last.is_none_or(|last| now < last)) && std::fs::create_dir_all(state_dir).is_ok() {
        let _ = std::fs::write(&path, now.to_string());
    }
    due.then_some("idle")
}

/// Starts one detached `bay live checkpoint <id>` in `cwd` (`crate::start::checkpoint`): the
/// hook never pushes and never waits. Whether it started.
fn start_checkpoint(cwd: &Path, id: &str) -> bool {
    let Ok(bay) = std::env::current_exe() else {
        return false;
    };
    let mut cmd = std::process::Command::new(bay);
    cmd.arg("-C")
        .arg(cwd)
        .args(["--json", "live", "checkpoint", id])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::platform::detach(&mut cmd);
    cmd.spawn().is_ok()
}

/// The session's claims in its trees (`DESIGN` § 11c, G101; `crate::claims`). A post-tool step
/// claims what it wrote: its edit-tool targets and, in a view, the files its line lists as
/// written, for the takeover window from now; a subagent's step claims for its parent's session.
/// The session's own stop releases its claims, unless ruling R7 refused that stop (the session
/// goes on working), and so does its end.
fn claims(
    repo: &Repo,
    payload: &Payload,
    view: Option<&Path>,
    line: &Value,
    state_dir: &Path,
    stop_refused: bool,
) {
    let Some(session) = payload.session.as_ref() else {
        return;
    };
    match payload.event {
        Event::PostToolUse | Event::PostToolBatch => {
            let mut targets = payload.edited.clone();
            if let (Some(view), Some(wrote)) = (view, line["wrote"].as_array()) {
                targets.extend(
                    wrote
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|path| view.join(path)),
                );
            }
            crate::claims::claim(
                &payload.cwd,
                &targets,
                session.as_str(),
                crate::live::unix_now(),
                repo.config.takeover().as_secs(),
                state_dir,
            );
        }
        Event::Stop if payload.agent.is_none() && !stop_refused => {
            crate::claims::release(session.as_str(), state_dir);
        }
        Event::SessionEnd => crate::claims::release(session.as_str(), state_dir),
        _ => {}
    }
}

/// The notices of `scope` (the view's change and the session's actor) for this event's line and
/// agent: at `post-tool` and `user-prompt-submit`, to the session's own agent only
/// (`crate::notice`). At the session's own `SessionStart`, the instruction files its tree is
/// behind trunk on (G155, `crate::notice::instructions`).
fn notices(
    repo: &Repo,
    scope: crate::notice::Scope<'_>,
    payload: &Payload,
    state_dir: &Path,
    line: &mut Value,
) -> String {
    if payload.event == Event::SessionStart && payload.agent.is_none() {
        let Some((text, behind)) = crate::notice::instructions(repo, &payload.cwd, state_dir)
        else {
            return String::new();
        };
        line["instructions_behind"] = behind;
        return text;
    }
    if payload.agent.is_some()
        || !matches!(
            payload.event,
            Event::PostToolBatch | Event::UserPromptSubmit
        )
    {
        return String::new();
    }
    let delivery = crate::notice::deliver(repo, scope, state_dir);
    for (key, value) in delivery.fields() {
        line[key] = value;
    }
    delivery.text
}

/// The session's own stop in a view: the change's state commit and whether it is a retention
/// cut, as the record read them.
struct AtStop {
    head: String,
    cut: bool,
}

/// The fields one step in the view of change `id` adds to its line: the files it wrote and, at
/// the session's stop (`at_stop`), whether the view holds uncheckpointed edits. Ruling R7's
/// refusal when the stop is refused.
fn step_fields(
    line: &mut Value,
    payload: &Payload,
    (view, state, state_dir): (&Path, &Path, &Path),
    step: crate::writes::Step,
    at_stop: Option<&AtStop>,
    id: &str,
) -> Option<String> {
    use crate::writes::{Edits, Wrote};
    let (wrote, edits) = match at_stop {
        Some(at) => {
            let (wrote, edits) = crate::writes::stop(view, state, &at.head, at.cut);
            (Some(wrote), Some(edits))
        }
        None => (crate::writes::step(view, state, step), None),
    };
    match wrote {
        Some(Wrote::Paths(paths)) => line["wrote"] = serde_json::json!(paths),
        Some(Wrote::Unknown(why)) => line["wrote_unknown"] = Value::from(why),
        None => {}
    }
    match edits? {
        Edits::Checkpointed => line["uncheckpointed"] = Value::from(false),
        Edits::Unknown(why) => {
            line["uncheckpointed"] = Value::Null;
            line["uncheckpointed_unknown"] = Value::from(why);
        }
        Edits::Uncheckpointed(digest) => {
            line["uncheckpointed"] = Value::from(true);
            if payload.stop_hook_active == Some(false) && refuse_once(state_dir, &digest) {
                line["state"] = Value::from("working");
                line["stop_refused"] = Value::from(true);
                return Some(stop_refusal(id));
            }
        }
    }
    None
}

/// The file that holds the digest of the edits the session's last refused stop was over.
const REFUSED_FILE: &str = "stop-refused";

/// Whether to refuse a stop over the edits `digest`: not when the session's last refused stop
/// was over the same edits, and not when bay cannot remember this refusal, so a session is
/// refused once per set of edits and never loops, whatever the product sends next.
fn refuse_once(state_dir: &Path, digest: &str) -> bool {
    let marker = state_dir.join(REFUSED_FILE);
    let mut last = String::new();
    if let Ok(file) = std::fs::File::open(&marker) {
        let _ = std::io::Read::read_to_string(&mut std::io::Read::take(file, 128), &mut last);
    }
    if last == digest {
        return false;
    }
    std::fs::create_dir_all(state_dir).is_ok() && std::fs::write(&marker, digest).is_ok()
}

/// Ruling R7's reason for refusing a stop: the view's edits are in no checkpoint, and the two
/// commands that put them in one.
fn stop_refusal(id: &str) -> String {
    format!(
        "bay: change {id}: the view holds edits that no checkpoint records. Checkpoint them \
         with `bay start --resume {id}`, or submit the change with `bay land`; then stop.\n"
    )
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

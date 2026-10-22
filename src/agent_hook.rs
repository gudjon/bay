//! The agent-product payload adapters behind `bay hook <event>` (`DESIGN` § 11, § 11a,
//! `ROADMAP` § 8).
//!
//! One entry, [`dispatch`], maps a hook's stdin to bay's events and runs what bay does there:
//! the session work record of every probed event in a repository but pre-tool
//! (`session::record`), the compact state at `session-start`, at `pre-tool` the claims of other
//! sessions in the same tree (`crate::claims`, G101), the shell deny list (G39) and the
//! stale-write judgement of an edit tool against the coordination cache (`crate::coord`, G8,
//! G23), the stop refusal at `stop` (ruling R7), and at `post-tool` and
//! `user-prompt-submit` the notices of the session's change (`crate::notice`, R6). Each product
//! has its own small payload module, which reads only the shapes recorded for it: `claude`
//! (`tests/fixtures/claude-code-2.1.302`) and `codex` (`tests/fixtures/codex-0.153.2`, G308).
//! An event with no recorded payload has no behaviour (ruling R16), and a field the recordings
//! do not show is never read. The product is the `[agent] product` of the config at trunk
//! ([`Product::configured`]), never a `bay hook` argument. A hook never blocks the session
//! except by the deny exit, a held path's deny, the stale-write refusal, the
//! one stop refusal and a `pre-tool` payload it cannot read (`HOOK_INPUT_INVALID`, ruling R-H1,
//! G59), so every other outcome exits 0: the events that only record allow an unreadable one.
//! Both products deny by exit 2 with the reason on stderr; Codex also runs the tool when its
//! hook crashes, hangs or exits otherwise (probed on 0.153.2), so there the deny is advisory and
//! the guard is `land` and the trunk hook.

mod claude;
mod codex;

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::result::Reason;
use crate::session::SessionId;

/// The agent product whose payloads `bay hook` reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Product {
    /// Claude Code: `[agent] product = "claude"`, and the reading when no config names Codex.
    Claude,
    /// Codex: `[agent] product = "codex"` (G308).
    Codex,
}

impl Product {
    /// The product the config at trunk of the repository that holds the payload's `cwd` (else
    /// `dir`) names: Codex for `[agent] product = "codex"`, else Claude Code, the one adapter
    /// bay had before G308, so an unreadable payload or config reads as before.
    #[must_use]
    pub fn configured(input: &str, dir: &Path) -> Self {
        let value: Value = serde_json::from_str(input).unwrap_or(Value::Null);
        let cwd = crate::session::hook_cwd(&value, dir);
        match crate::repo::Repo::open(&cwd) {
            Ok(repo) if repo.config.codex_adapter() => Self::Codex,
            _ => Self::Claude,
        }
    }

    /// The bay event of the payload's `hook_event_name`.
    fn event(self, name: &str) -> Event {
        match self {
            Self::Claude => Event::from_name(name),
            Self::Codex => codex::event(name),
        }
    }

    /// The product events bay installs, with the `bay hook` name each one calls.
    fn entries(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Claude => crate::agent_settings::CLAUDE_EVENTS,
            Self::Codex => crate::agent_settings::CODEX_EVENTS,
        }
    }

    /// The event that ends a step, whose writes bay records and whose notices it gives: Claude
    /// Code's `PostToolBatch`; Codex has none, so its `PostToolUse`.
    #[must_use]
    pub fn step_event(self) -> Event {
        match self {
            Self::Claude => Event::PostToolBatch,
            Self::Codex => Event::PostToolUse,
        }
    }

    /// The edit targets a post-tool payload names.
    fn edited(self, event: Event, value: &Value) -> Vec<PathBuf> {
        match self {
            Self::Claude => claude::edited(event, value),
            Self::Codex => codex::edited(event, value),
        }
    }

    /// What a `pre-tool` payload (a JSON object) calls, or why bay cannot judge it (ruling R-H1,
    /// G59): a call with no string `tool_name`, or a shell call with no string
    /// `tool_input.command`, the shell's shape in both products' recordings.
    fn call(self, value: &Value) -> Result<Call, &'static str> {
        let Some(tool) = value["tool_name"].as_str() else {
            return Err("has no string tool_name");
        };
        if tool == "Bash" {
            return value["tool_input"]["command"]
                .as_str()
                .map(|command| Call::Shell(command.to_owned()))
                .ok_or("is a Bash call with no string tool_input.command");
        }
        let targets = match self {
            Self::Claude => claude::edit_targets(tool, value),
            Self::Codex => codex::edit_targets(tool, value)?,
        };
        Ok(targets.map_or(Call::Other, Call::Edit))
    }

    /// `outcome` in the product's output form. Claude Code reads it as made. No Codex recording
    /// shows Codex reading a hook's stdout (R16), and output it cannot parse fails the hook, so
    /// at exit 0 Codex gets bay's lines on stderr only; `session-start`'s context stays plain
    /// text on stdout, and a deny keeps its `bay.result/1`.
    fn deliver(self, event: &str, outcome: Outcome) -> Outcome {
        if self == Self::Codex && outcome.exit == 0 && event != "session-start" {
            return Outcome {
                stdout: String::new(),
                ..outcome
            };
        }
        outcome
    }
}

/// A `pre-tool` call, as bay judges it.
#[derive(Debug)]
enum Call {
    /// The shell, with its command.
    Shell(String),
    /// An edit tool, with the targets its payload names; none when it names none bay reads.
    Edit(Vec<PathBuf>),
    /// Any other tool: not judged.
    Other,
}

/// A hook event, by the payload's `hook_event_name` in Claude Code's names; each product's
/// module maps its own names to these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PostToolBatch,
    Notification,
    Stop,
    SubagentStart,
    SubagentStop,
    TeammateIdle,
    FileChanged,
    PreCompact,
    SessionEnd,
    /// An event with no recorded payload (`CwdChanged`, `WorktreeCreate`, `WorktreeRemove`,
    /// `StopFailure`, or a name bay does not know): no behaviour until it is probed (R16).
    Unprobed,
}

impl Event {
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name {
            "SessionStart" => Self::SessionStart,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "PreToolUse" => Self::PreToolUse,
            "PostToolUse" => Self::PostToolUse,
            "PostToolBatch" => Self::PostToolBatch,
            "Notification" => Self::Notification,
            "Stop" => Self::Stop,
            "SubagentStart" => Self::SubagentStart,
            "SubagentStop" => Self::SubagentStop,
            "TeammateIdle" => Self::TeammateIdle,
            "FileChanged" => Self::FileChanged,
            "PreCompact" => Self::PreCompact,
            "SessionEnd" => Self::SessionEnd,
            _ => Self::Unprobed,
        }
    }

    /// The event's name in the work record (`INTERFACES` § 5a): `PostToolBatch` →
    /// `post_tool_batch`.
    #[must_use]
    pub fn record_name(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::UserPromptSubmit => "user_prompt_submit",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::PostToolBatch => "post_tool_batch",
            Self::Notification => "notification",
            Self::Stop => "stop",
            Self::SubagentStart => "subagent_start",
            Self::SubagentStop => "subagent_stop",
            Self::TeammateIdle => "teammate_idle",
            Self::FileChanged => "file_changed",
            Self::PreCompact => "pre_compact",
            Self::SessionEnd => "session_end",
            Self::Unprobed => "unprobed",
        }
    }

    /// The session's state after this event, where § 11a's wiring names one. A teammate's
    /// `TeammateIdle` is recorded as idle; it is not a mailbox.
    #[must_use]
    pub fn state_after(self) -> Option<&'static str> {
        match self {
            Self::UserPromptSubmit | Self::PostToolBatch => Some("working"),
            Self::Notification => Some("waiting"),
            Self::Stop | Self::TeammateIdle => Some("idle"),
            Self::SessionEnd => Some("ended"),
            _ => None,
        }
    }

    /// What the event does with the view's Git status for the write record (§ 11a): an agent's
    /// start (the session's, a subagent's) keeps the baseline its first step diffs against, so
    /// what the view held before is not its write; the product's post-tool and stop events list
    /// the step's writes.
    #[must_use]
    pub fn writes(self) -> Option<crate::writes::Step> {
        use crate::writes::Step;
        match self {
            Self::SessionStart | Self::SubagentStart => Some(Step::Baseline),
            Self::PostToolBatch | Self::Stop => Some(Step::Diff),
            Self::SubagentStop => Some(Step::DiffAndForget),
            _ => None,
        }
    }
}

/// One hook payload, as far as the recorded shapes show it.
#[derive(Debug)]
pub struct Payload {
    /// The product whose payload this is.
    pub product: Product,
    pub event: Event,
    /// `session_id`, when it names a file and nothing else. A subagent keeps its parent's.
    pub session: Option<SessionId>,
    /// A subagent's `agent_id`.
    pub agent: Option<String>,
    /// The payload's `cwd`, else the directory bay runs in.
    pub cwd: PathBuf,
    /// A `Stop` payload's `stop_hook_active`: the product already continued the session once
    /// for a stop hook.
    pub stop_hook_active: Option<bool>,
    /// The targets of the step's edit-tool calls, as the product's module reads them
    /// (`claude::edited`, `codex::edited`). As the payload gives them: absolute, or relative to
    /// `cwd`.
    pub edited: Vec<PathBuf>,
    /// The product's `permission_mode` (fixtures 02-15, e.g. `bypassPermissions`), when it is
    /// one plain word: the value reaches the fleet map, so any other text is dropped (G153).
    pub permission_mode: Option<String>,
}

impl Payload {
    /// Reads `input` (the hook's stdin) from `product` for `bay hook <bay_event>`. The
    /// payload's `hook_event_name` names the event; without one, the installed entry's
    /// `bay hook` name does. `None` when the input is not a JSON object.
    #[must_use]
    pub fn parse(product: Product, bay_event: &str, input: &str, dir: &Path) -> Option<Self> {
        let value: Value = serde_json::from_str(input).ok()?;
        if !value.is_object() {
            return None;
        }
        let event = value["hook_event_name"].as_str().map_or_else(
            || {
                product
                    .entries()
                    .iter()
                    .find(|(_, name)| *name == bay_event)
                    .map_or(Event::Unprobed, |(name, _)| product.event(name))
            },
            |name| product.event(name),
        );
        Some(Self {
            product,
            event,
            session: value["session_id"].as_str().and_then(SessionId::parse),
            agent: value["agent_id"].as_str().map(str::to_owned),
            cwd: crate::session::hook_cwd(&value, dir),
            stop_hook_active: value["stop_hook_active"].as_bool(),
            edited: product.edited(event, &value),
            permission_mode: value["permission_mode"]
                .as_str()
                .filter(|mode| {
                    (1..=32).contains(&mode.len()) && mode.bytes().all(|b| b.is_ascii_alphabetic())
                })
                .map(str::to_owned),
        })
    }

    /// Whether this event ends a step of its product ([`Product::step_event`]).
    #[must_use]
    pub fn ends_step(&self) -> bool {
        self.event == self.product.step_event()
    }

    /// The session's state after this event ([`Event::state_after`]): `working` after a step.
    #[must_use]
    pub fn state_after(&self) -> Option<&'static str> {
        if self.ends_step() {
            Some("working")
        } else {
            self.event.state_after()
        }
    }

    /// What the event does with the view's Git status ([`Event::writes`]): a step's end lists
    /// the step's writes.
    #[must_use]
    pub fn writes(&self) -> Option<crate::writes::Step> {
        if self.ends_step() {
            Some(crate::writes::Step::Diff)
        } else {
            self.event.writes()
        }
    }
}

/// What `bay hook` returns to the product: its exit code and what it prints.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub exit: u8,
    pub stdout: String,
    pub stderr: String,
}

/// `bay hook <event>` with `input` on stdin, run in `dir`, for the configured product. The
/// exit protocol of both products (`INTERFACES` § 1): exit 2 denies, with the reason on stderr.
#[must_use]
pub fn dispatch(event: &str, input: &str, dir: &Path) -> Outcome {
    // G54: the idle wait an asynchronous hook runs: exit 2 (the wake) at the first accepted talk.
    if event == "talk-wait" {
        return talk_wait(dir);
    }
    let product = Product::configured(input, dir);
    let outcome = dispatch_as(product, event, input, dir);
    product.deliver(event, outcome)
}

/// [`dispatch`] of a payload from `product`: one event handling for every product.
fn dispatch_as(product: Product, event: &str, input: &str, dir: &Path) -> Outcome {
    // G54: a live session accepts its talks at most once per poll interval; what arrives is
    // told by the talk notice at a later hook (G147).
    if matches!(event, "user-prompt-submit" | "post-tool")
        && let Ok(repo) = crate::repo::Repo::open(dir)
    {
        let _ = crate::talk::poll::live(&repo, crate::live::unix_now());
    }
    // `PreToolUse` is the blocking path (`onFailure: "block"`): § 11a gives it only the refusal,
    // and the same step's end (`PostToolBatch`, Codex's `PostToolUse`) records it.
    let mut recorded = crate::session::Recorded::default();
    let mut product_event = Event::Unprobed;
    if let Some(payload) = Payload::parse(product, event, input, dir)
        && !matches!(payload.event, Event::Unprobed | Event::PreToolUse)
    {
        product_event = payload.event;
        recorded = crate::session::record(&payload);
    }
    // Ruling R7: a stop over uncheckpointed edits is refused once; exit 2 keeps the session
    // going, with the reason as its next instruction.
    if let Some(reason) = recorded.stop_refused {
        return Outcome {
            exit: 2,
            stdout: String::new(),
            stderr: reason,
        };
    }
    // `SessionStart`'s stdout is the session's context: the compact state, then the notice
    // its line recorded (G155).
    if event == "session-start" {
        let mut stdout = crate::session::session_start(dir, input);
        stdout.push_str(&recorded.notice);
        return Outcome {
            stdout,
            ..Outcome::default()
        };
    }
    if !recorded.notice.is_empty() {
        return notice(product_event, recorded.notice);
    }
    if event != "pre-tool" {
        return shell_deny(product, event, input, dir);
    }
    pre_tool(product, input, dir)
}

/// `pre-tool`: a payload bay cannot read is denied (G59); else the claims of other sessions
/// (G101), then the stale-write judgement of an edit tool or the shell deny list (G39).
fn pre_tool(product: Product, input: &str, dir: &Path) -> Outcome {
    let (value, call) = match pre_tool_call(product, input) {
        Ok(read) => read,
        Err(why) => return invalid_payload(product, input, dir, why),
    };
    let unknown = match pre_tool_claim(product, input, dir, &value, &call) {
        Ok(unknown) => unknown,
        Err(denied) => return denied,
    };
    let outcome = match &call {
        Call::Edit(targets) => pre_tool_write(product, input, dir, &value, targets),
        Call::Shell(_) | Call::Other => shell_deny(product, "pre-tool", input, dir),
    };
    match unknown {
        Some(unreadable) if outcome.exit == 0 => {
            claims_unknown(product, input, dir, &unreadable, &outcome)
        }
        _ => outcome,
    }
}

/// `bay hook talk-wait` (G54): poll this actor's talks for at most an hour, on the poll schedule;
/// exit 2 with one line per accepted talk when one arrives, exit 0 when none did.
fn talk_wait(dir: &Path) -> Outcome {
    const MAX_WAIT_S: u64 = 3600;
    let Ok(repo) = crate::repo::Repo::open(dir) else {
        return Outcome::default();
    };
    let (exit, outcome) = crate::talk::poll::wait(&repo, MAX_WAIT_S);
    let stderr = if exit == 2 {
        let talks: Vec<String> = outcome["accepted"]["acked"]
            .as_array()
            .map(|acked| {
                acked
                    .iter()
                    .filter_map(|id| id.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        format!(
            "bay: a talk arrived: {} (bay status shows it)\n",
            talks.join(", ")
        )
    } else {
        String::new()
    };
    Outcome {
        exit,
        stdout: outcome.to_string(),
        stderr,
    }
}

/// The shell deny list's judgement of `event` (G39): exit 2 with the reason, recorded by the
/// command's shape; else the default outcome.
fn shell_deny(product: Product, event: &str, input: &str, dir: &Path) -> Outcome {
    let Some(denial) = crate::deny::judge_hook(event, input, dir) else {
        return Outcome::default();
    };
    // The row names the deny-list shape and its wrapper, never the command (R-H2, H3): a
    // command line can carry a secret, and the record is payload-free.
    let reason = Reason::ShellDenied;
    let mut refused = serde_json::json!({ "reason": reason.code(), "shape": denial.shape });
    if let Some(wrapped) = denial.wrapped {
        refused["wrapped"] = Value::from(wrapped);
    }
    record_pre_tool(
        product,
        input,
        dir,
        serde_json::json!({
            "state": format!("blocked({})", reason.code()),
            "refused": refused,
            "verdict": deny_verdict(reason),
        }),
    );
    Outcome {
        exit: 2,
        stdout: String::new(),
        stderr: format!("{}\n", denial.reason),
    }
}

/// An AGENT-HOOKS-0.1 `deny` verdict (§ 5) for a judging `pre-tool` row of the work record
/// (ruling R-H2, G60). `reason` is a bay reason code, never a `host_error:*` one; the verdict
/// fields bay has no facts for (`transform`, `approval`, `evidence`, `result_labels`) are left
/// out. `bay.result/1` keeps its own words.
fn deny_verdict(reason: Reason) -> Value {
    serde_json::json!({ "decision": "deny", "reason": reason.code() })
}

/// An AGENT-HOOKS-0.1 `allow` verdict with a warning, AH's words for bay's notice (§ 5.1: there
/// is no `warn` decision): the write is allowed, and `reason` says why it was not judged.
fn allow_verdict_with_warning(reason: Reason) -> Value {
    serde_json::json!({
        "decision": "allow",
        "reason": reason.code(),
        "warnings": [{ "reason": reason.code() }],
    })
}

/// Appends a `pre-tool` row with `extra` to the session's work record, when the payload names a
/// session (`session::record_with`). A pre-tool line is never a stop, so it carries no stop
/// refusal.
fn record_pre_tool(product: Product, input: &str, dir: &Path, extra: Value) {
    if let (Some(payload), Value::Object(extra)) =
        (Payload::parse(product, "pre-tool", input, dir), extra)
    {
        let _ = crate::session::record_with(&payload, &extra);
    }
}

/// The `pre-tool` payload and what it calls, when bay can judge it (ruling R-H1, G59): a JSON
/// object whose call the product's module reads ([`Product::call`]). Else why not. An empty
/// stdin and one that is not UTF-8 (read as empty) are not JSON.
fn pre_tool_call(product: Product, input: &str) -> Result<(Value, Call), &'static str> {
    let Ok(value) = serde_json::from_str::<Value>(input) else {
        return Err(if input.trim().is_empty() {
            "is missing"
        } else {
            "is not JSON"
        });
    };
    if !value.is_object() {
        return Err("is not a JSON object");
    }
    let call = product.call(&value)?;
    Ok((value, call))
}

/// A `pre-tool` payload bay cannot read is denied (ruling R-H1, AGENT-HOOKS-0.1 § 6.3), never
/// allowed: a product release that renames a field would otherwise turn the deny list and the
/// stale-write refusal off without a word. Exit 2 with one line on stderr, the deny exit of the
/// product's protocol; stdout holds the `bay.result/1` of the refusal, for a caller that reads
/// one (Claude Code reads stderr at exit 2). A JSON object that names a session is recorded as
/// a deny in its work record, with `why` and none of the payload.
fn invalid_payload(product: Product, input: &str, dir: &Path, why: &str) -> Outcome {
    let reason = Reason::HookInputInvalid;
    record_pre_tool(
        product,
        input,
        dir,
        serde_json::json!({
            "state": format!("blocked({})", reason.code()),
            "refused": { "reason": reason.code(), "why": why },
            "verdict": deny_verdict(reason),
        }),
    );
    let result = crate::result::BayResult::blocked(reason)
        .with_detail(serde_json::json!({ "event": "pre-tool", "why": why }));
    Outcome {
        exit: 2,
        stdout: result.to_json().unwrap_or_default(),
        stderr: format!(
            "bay: refused {}: the pre-tool payload {why}, so bay denies the tool call; \
             `bay doctor` checks the hook entries and `bay update` installs a newer bay\n",
            reason.code()
        ),
    }
}

/// The notices `post-tool` or `user-prompt-submit` gives the agent (`crate::notice`, ruling R6).
/// § 11 names `additionalContext`, but the recorded payloads show only the hooks' input, so no
/// probe has shown this event delivering that field on 2.1.302 (R16): the same text also goes
/// on stderr at exit 0, the probed exit-0 path (ruling R-B3-6), and the session's row names
/// each notice.
fn notice(event: Event, text: String) -> Outcome {
    let name = match event {
        Event::UserPromptSubmit => "UserPromptSubmit",
        _ => "PostToolBatch",
    };
    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": name,
            "additionalContext": text,
        },
    });
    Outcome {
        exit: 0,
        stdout: format!("{output}\n"),
        stderr: text,
    }
}

/// `pre-tool` on an edit tool, or on Bash when the command names a path: denied when another
/// live session of the target's tree holds that path (`DESIGN` § 11c, G101; `crate::claims`).
/// The deny exits 2 with one line naming the reason code, the holder and the path, and its row
/// in the denied session's work record is a G60 verdict. A payload with no session id is not
/// the holder's. `Ok(None)` passes it to the next judgement; `Ok(Some(_))` does too, but a
/// claims file could not be read, so the write was not judged here ([`claims_unknown`]).
fn pre_tool_claim(
    product: Product,
    input: &str,
    dir: &Path,
    value: &Value,
    call: &Call,
) -> Result<Option<crate::claims::Unreadable>, Outcome> {
    let cwd = crate::session::hook_cwd(value, dir);
    let session = value["session_id"].as_str().and_then(SessionId::parse);
    let session = session.as_ref().map(SessionId::as_str);
    let now = crate::live::unix_now();
    let judged = match call {
        Call::Edit(targets) => held_target(&cwd, targets, session, now),
        Call::Shell(command) => crate::claims::held_by_command(&cwd, command, session, now),
        Call::Other => Ok(None),
    };
    let held = match judged {
        Ok(Some(held)) => held,
        Ok(None) => return Ok(None),
        Err(unreadable) => return Ok(Some(unreadable)),
    };
    let reason = Reason::PathHeld;
    let until = crate::clock::rfc3339_of(held.deadline_s);
    record_pre_tool(
        product,
        input,
        dir,
        serde_json::json!({
            "state": format!("blocked({})", reason.code()),
            "refused": {
                "reason": reason.code(),
                "holder": held.holder,
                "path": held.path,
                "deadline": until,
            },
            "verdict": deny_verdict(reason),
        }),
    );
    Err(Outcome {
        exit: 2,
        stdout: String::new(),
        stderr: format!(
            "bay: refused {}: {} is held by session {} in this tree until {until}; write \
             another path, or wait until that session stops\n",
            reason.code(),
            held.path,
            held.holder,
        ),
    })
}

/// The first of an edit's `targets` another live session holds ([`crate::claims::held`]); else
/// the first claims file that could not be read; else none. An edit that names no target is
/// not judged here.
fn held_target(
    cwd: &Path,
    targets: &[PathBuf],
    session: Option<&str>,
    now: u64,
) -> Result<Option<crate::claims::Held>, crate::claims::Unreadable> {
    let mut unreadable = None;
    for target in targets {
        match crate::claims::held(cwd, target, session, now) {
            Ok(Some(held)) => return Ok(Some(held)),
            Ok(None) => {}
            Err(why) => {
                unreadable.get_or_insert(why);
            }
        }
    }
    unreadable.map_or(Ok(None), Err)
}

/// A `pre-tool` call the later judgements allowed (`outcome`, exit 0) whose claims file could
/// not be read: allowed with a `COVERAGE_UNKNOWN` notice (ruling R5, G101), never silently. The
/// notice goes in `additionalContext`, beside any notice the write already had, and on stderr;
/// the session's row records it with its G60 verdict.
fn claims_unknown(
    product: Product,
    input: &str,
    dir: &Path,
    unreadable: &crate::claims::Unreadable,
    outcome: &Outcome,
) -> Outcome {
    let reason = Reason::CoverageUnknown;
    let file = unreadable.file.display().to_string();
    record_pre_tool(
        product,
        input,
        dir,
        serde_json::json!({
            "notice": {
                "reason": reason.code(),
                "path": unreadable.path,
                "claims_file": file,
                "why": unreadable.why,
            },
            "verdict": allow_verdict_with_warning(reason),
        }),
    );
    let what = unreadable
        .path
        .as_deref()
        .map_or_else(|| "this command".to_owned(), str::to_owned);
    let line = format!(
        "bay: {} for {what}: the claims file {file} {}, so bay cannot tell whether another \
         session in this tree holds what it writes; the write is allowed unchecked, and the \
         next claim replaces that file\n",
        reason.code(),
        unreadable.why,
    );
    let earlier: Value = serde_json::from_str(&outcome.stdout).unwrap_or(Value::Null);
    let context = earlier["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .map_or_else(|| line.clone(), |text| format!("{line}{text}"));
    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": context,
        },
    });
    Outcome {
        exit: 0,
        stdout: format!("{output}\n"),
        stderr: format!("{line}{}", outcome.stderr),
    }
}

/// What `pre-tool` does not judge (G306), as `bay help --json` declares it: a shell command is
/// judged only by the deny list (G39); an edit whose payload names no target is an edit to an
/// unknown path; a write against a coordination cache older than `[hooks] cache_max_age_s` is
/// allowed with `COVERAGE_UNKNOWN` (ruling R5).
pub const CANNOT_SEE: &[&str] = &[
    "shell writes, judged only by the deny list",
    "an edit whose payload names no target (no file_path, or a patch with no file header)",
    "writes while the coordination cache is older than [hooks] cache_max_age_s, allowed with COVERAGE_UNKNOWN",
];

/// `pre-tool` on an edit tool: the write of each of its `targets` judged against the
/// coordination cache ([`judge_targets`]). An edit that names no target (a `NotebookEdit`, an
/// `apply_patch` with no file header) is an edit to an unknown path. A refusal exits 2, the
/// deny exit of the shell deny list (G39); a `COVERAGE_UNKNOWN` notice allows the write with
/// `additionalContext` (ruling R5) and the same line on stderr, since no probe has yet shown
/// `PreToolUse` delivering that field. Both are recorded in the session's row.
fn pre_tool_write(
    product: Product,
    input: &str,
    dir: &Path,
    value: &Value,
    targets: &[PathBuf],
) -> Outcome {
    let cwd = crate::session::hook_cwd(value, dir);
    let verdict = judge_targets(&cwd, targets, SystemTime::now());
    let record = |extra: Value| record_pre_tool(product, input, dir, extra);
    match verdict {
        crate::coord::Verdict::Allow => Outcome::default(),
        crate::coord::Verdict::Refuse {
            reason,
            line,
            detail,
        } => {
            record(serde_json::json!({
                "state": format!("blocked({})", reason.code()),
                "refused": detail,
                "verdict": deny_verdict(reason),
            }));
            Outcome {
                exit: 2,
                stdout: String::new(),
                stderr: format!("{line}\n"),
            }
        }
        crate::coord::Verdict::Notice { text, detail } => {
            record(serde_json::json!({
                "notice": detail,
                "verdict": allow_verdict_with_warning(Reason::CoverageUnknown),
            }));
            let output = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "additionalContext": text,
                },
            });
            Outcome {
                exit: 0,
                stdout: format!("{output}\n"),
                stderr: format!("{text}\n"),
            }
        }
    }
}

/// The coordination cache's verdict on writing `targets` at `now` ([`crate::coord::judge`]):
/// the first refusal of a target; else its first notice; else allowed. No target is an edit to
/// an unknown path.
fn judge_targets(cwd: &Path, targets: &[PathBuf], now: SystemTime) -> crate::coord::Verdict {
    use crate::coord::{Verdict, judge};
    if targets.is_empty() {
        return judge(cwd, None, now);
    }
    let mut notice = None;
    for target in targets {
        match judge(cwd, Some(target.as_path()), now) {
            Verdict::Allow => {}
            refused @ Verdict::Refuse { .. } => return refused,
            noticed @ Verdict::Notice { .. } => {
                notice.get_or_insert(noticed);
            }
        }
    }
    notice.unwrap_or(Verdict::Allow)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{Event, Payload, Product};

    #[test]
    fn the_payload_names_the_event_and_the_entry_name_is_the_fallback() {
        let dir = Path::new("/nonexistent");
        let parse = |event, input| Payload::parse(Product::Claude, event, input, dir);
        let named = parse("post-tool", r#"{"hook_event_name": "Stop"}"#).unwrap();
        assert_eq!(named.event, Event::Stop);
        let bare = parse("teammate-idle", "{}").unwrap();
        assert_eq!(bare.event, Event::TeammateIdle);
        let unprobed = parse("worktree-create", "{}").unwrap();
        assert_eq!(unprobed.event, Event::Unprobed);
        assert!(parse("stop", "[]").is_none());
    }

    #[test]
    fn a_session_id_that_is_not_a_file_name_is_dropped() {
        let dir = Path::new("/nonexistent");
        let payload =
            Payload::parse(Product::Claude, "stop", r#"{"session_id": "../x"}"#, dir).unwrap();
        assert!(payload.session.is_none());
    }

    #[test]
    fn codex_reads_only_its_recorded_events_and_its_post_tool_ends_a_step() {
        // G308, R16: Codex has no `PostToolBatch`, so its `PostToolUse` is the step's end; a
        // Claude-only event name has no behaviour.
        let dir = Path::new("/nonexistent");
        let parse = |event, input| Payload::parse(Product::Codex, event, input, dir);
        let step = parse("post-tool", r#"{"hook_event_name": "PostToolUse"}"#).unwrap();
        assert!(step.ends_step());
        assert_eq!(step.state_after(), Some("working"));
        let bare = parse("post-tool", "{}").unwrap();
        assert_eq!(bare.event, Event::PostToolUse);
        let claude_only = parse("post-tool", r#"{"hook_event_name": "PostToolBatch"}"#).unwrap();
        assert_eq!(claude_only.event, Event::Unprobed);
        let patch = r#"{"hook_event_name": "PostToolUse", "tool_name": "apply_patch",
            "tool_input": {"command": "*** Begin Patch\n*** Add File: a.txt\n+a\n*** End Patch"}}"#;
        assert_eq!(
            parse("post-tool", patch).unwrap().edited,
            vec![PathBuf::from("a.txt")]
        );
        // Claude Code's `PostToolUse` does not end a step: its `PostToolBatch` does.
        let claude = Payload::parse(
            Product::Claude,
            "post-tool",
            r#"{"hook_event_name": "PostToolUse"}"#,
            dir,
        )
        .unwrap();
        assert!(!claude.ends_step());
    }
}

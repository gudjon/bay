//! The Claude Code payload adapter behind `bay hook <event>` (`DESIGN` § 11, § 11a, `ROADMAP` § 8).
//!
//! One entry, [`dispatch`], maps a hook's stdin to bay's events and runs what bay does there:
//! the session work record of every probed event in a repository but pre-tool
//! (`session::record`), the compact state at `session-start`, at `pre-tool` the shell deny list
//! (G39) and the stale-write judgement of an edit tool against the coordination cache
//! (`crate::coord`, G8, G23), and the stop refusal at `stop` (ruling R7). The adapter relies
//! only on the payload shapes recorded in `tests/fixtures/claude-code-2.1.302`: an event with no
//! recorded payload has no behaviour (ruling R16), and a field the recordings do not show is
//! never read: an edit tool's target is `tool_input.file_path`, the field fixture 03 shows. A
//! hook never blocks the session except by the deny exit, the stale-write refusal and the one
//! stop refusal, so every other outcome exits 0.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::session::SessionId;

/// A Claude Code hook event, by the payload's `hook_event_name`.
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
}

impl Payload {
    /// Reads `input` (the hook's stdin) for `bay hook <bay_event>`. The payload's
    /// `hook_event_name` names the event; without one, the installed entry's `bay hook` name
    /// does. `None` when the input is not a JSON object.
    #[must_use]
    pub fn parse(bay_event: &str, input: &str, dir: &Path) -> Option<Self> {
        let value: Value = serde_json::from_str(input).ok()?;
        if !value.is_object() {
            return None;
        }
        let event = value["hook_event_name"].as_str().map_or_else(
            || {
                crate::agent_settings::CLAUDE_EVENTS
                    .iter()
                    .find(|(_, name)| *name == bay_event)
                    .map_or(Event::Unprobed, |(product, _)| Event::from_name(product))
            },
            Event::from_name,
        );
        Some(Self {
            event,
            session: value["session_id"].as_str().and_then(SessionId::parse),
            agent: value["agent_id"].as_str().map(str::to_owned),
            cwd: crate::session::hook_cwd(&value, dir),
            stop_hook_active: value["stop_hook_active"].as_bool(),
        })
    }
}

/// What `bay hook` returns to the product: its exit code and what it prints.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub exit: u8,
    pub stdout: String,
    pub stderr: String,
}

/// `bay hook <event>` with `input` on stdin, run in `dir`. Claude Code's exit protocol
/// (`INTERFACES` § 1): exit 2 denies, with the reason on stderr.
#[must_use]
pub fn dispatch(event: &str, input: &str, dir: &Path) -> Outcome {
    // `PreToolUse` is the blocking path (`onFailure: "block"`): § 11a gives it only the refusal,
    // and the same step's `PostToolBatch` records it.
    let mut stop_refused = None;
    if let Some(payload) = Payload::parse(event, input, dir)
        && !matches!(payload.event, Event::Unprobed | Event::PreToolUse)
    {
        stop_refused = crate::session::record(&payload);
    }
    // Ruling R7: a stop over uncheckpointed edits is refused once; exit 2 keeps the session
    // going, with the reason as its next instruction.
    if let Some(reason) = stop_refused {
        return Outcome {
            exit: 2,
            stdout: String::new(),
            stderr: reason,
        };
    }
    if event == "pre-tool"
        && let Some(outcome) = pre_tool_write(input, dir)
    {
        return outcome;
    }
    if event == "session-start" {
        return Outcome {
            stdout: crate::session::session_start(dir, input),
            ..Outcome::default()
        };
    }
    match crate::deny::judge_hook(event, input, dir) {
        Some(reason) => Outcome {
            exit: 2,
            stdout: String::new(),
            stderr: format!("{reason}\n"),
        },
        None => Outcome::default(),
    }
}

/// The edit tools whose target `pre-tool` judges (§ 11a's `PreToolUse` row).
const EDIT_TOOLS: [&str; 3] = ["Edit", "Write", "NotebookEdit"];

/// `pre-tool` on an edit tool: the write judged against the coordination cache. `None` when the
/// payload is not an edit tool's, so the deny list judges it. The target is
/// `tool_input.file_path`, the field probed payload 03 shows; a payload without it (a
/// `NotebookEdit` names its target in a field no probe recorded, R16) is an edit to an unknown
/// path. A refusal exits 2, the deny exit of the shell deny list (G39); a `COVERAGE_UNKNOWN`
/// notice allows the write with `additionalContext` (ruling R5) and the same line on stderr,
/// since no probe has yet shown `PreToolUse` delivering that field. Both are recorded in the
/// session's row.
fn pre_tool_write(input: &str, dir: &Path) -> Option<Outcome> {
    let value: Value = serde_json::from_str(input).ok()?;
    let tool = value["tool_name"].as_str()?;
    if !EDIT_TOOLS.contains(&tool) {
        return None;
    }
    let file = value["tool_input"]["file_path"].as_str().map(PathBuf::from);
    let cwd = crate::session::hook_cwd(&value, dir);
    let verdict = crate::coord::judge(&cwd, file.as_deref(), std::time::SystemTime::now());
    let record = |extra: Value| {
        if let (Some(payload), Value::Object(extra)) =
            (Payload::parse("pre-tool", input, dir), extra)
        {
            // A pre-tool line is never a stop, so it carries no stop refusal.
            let _ = crate::session::record_with(&payload, &extra);
        }
    };
    Some(match verdict {
        crate::coord::Verdict::Allow => Outcome::default(),
        crate::coord::Verdict::Refuse {
            reason,
            line,
            detail,
        } => {
            record(serde_json::json!({
                "state": format!("blocked({})", reason.code()),
                "refused": detail,
            }));
            Outcome {
                exit: 2,
                stdout: String::new(),
                stderr: format!("{line}\n"),
            }
        }
        crate::coord::Verdict::Notice { text, detail } => {
            record(serde_json::json!({ "notice": detail }));
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
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Event, Payload};

    #[test]
    fn the_payload_names_the_event_and_the_entry_name_is_the_fallback() {
        let dir = Path::new("/nonexistent");
        let named = Payload::parse("post-tool", r#"{"hook_event_name": "Stop"}"#, dir).unwrap();
        assert_eq!(named.event, Event::Stop);
        let bare = Payload::parse("teammate-idle", "{}", dir).unwrap();
        assert_eq!(bare.event, Event::TeammateIdle);
        let unprobed = Payload::parse("worktree-create", "{}", dir).unwrap();
        assert_eq!(unprobed.event, Event::Unprobed);
        assert!(Payload::parse("stop", "[]", dir).is_none());
    }

    #[test]
    fn a_session_id_that_is_not_a_file_name_is_dropped() {
        let dir = Path::new("/nonexistent");
        let payload = Payload::parse("stop", r#"{"session_id": "../x"}"#, dir).unwrap();
        assert!(payload.session.is_none());
    }
}

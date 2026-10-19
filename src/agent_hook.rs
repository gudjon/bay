//! The Claude Code payload adapter behind `bay hook <event>` (`DESIGN` § 11, § 11a, `ROADMAP` § 8).
//!
//! One entry, [`dispatch`], maps a hook's stdin to bay's events and runs what bay does there:
//! the session work record of every probed event in a repository but pre-tool
//! (`session::record`), the compact state at `session-start`, and the shell deny list at
//! `pre-tool` (G39). The adapter relies only
//! on the payload shapes recorded in `tests/fixtures/claude-code-2.1.302`: an event with no
//! recorded payload has no behaviour (ruling R16), and a field the recordings do not show is
//! never read. A hook never blocks the session except by the deny exit, so every other outcome
//! exits 0.

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
    if let Some(payload) = Payload::parse(event, input, dir)
        && !matches!(payload.event, Event::Unprobed | Event::PreToolUse)
    {
        crate::session::record(&payload);
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

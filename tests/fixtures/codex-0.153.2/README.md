# Codex CLI 0.153.2 hook payloads

Recorded on 2026-10-18 from headless `codex exec` runs in throwaway
repositories, redacted (no machine paths, session, turn or agent ids, prompts, assistant text or
encrypted payloads).
Input for a `bay hook codex` adapter (`DESIGN` § 11, § 11a).

- `-main`: an `apply_patch` add, a shell write, and a shell command denied by a hook (exit 2).
- `-subagent`: a delegated agent (`collaborationspawn_agent`), with `SubagentStart`/`SubagentStop`
  and the subagent's own `PreToolUse` carrying `agent_id`.
- `-unknownev`: the one event that fired when Claude-only event names were also configured.

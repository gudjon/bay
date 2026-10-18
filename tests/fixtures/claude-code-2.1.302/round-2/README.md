# Claude Code 2.1.302 hook payloads, round 2

Recorded on 2026-10-18, redacted (no machine paths, session ids or assistant text).

- `00-Notification-interactive.json`: a permission prompt in an interactive session.
- `01`–`10` (`-inter`): an interactive session with agent teams on and one teammate. The
  teammate is its own session: its own `SessionStart` (with `agent_type`), `Stop`, and
  `TeammateIdle` (`teammate_name`, `team_name`); no `SubagentStart` for it.
- `11`–`14` (`-filechanged`): `FileChanged` for a literal `matcher` file that existed at session
  start, written by an outside process and by the agent's own shell (`event: "change"`).
- `agents-json-sample.json`: three rows of `claude agents --json`, identifying fields redacted.

# Claude Code 2.1.302 hook payloads, round 3 (interactive)

Recorded on 2026-10-18 in one interactive session driven in a detached `tmux`, redacted (no
machine paths, session or prompt ids, workspace).

- `01`, `07`: `SessionStart` with `source: "startup"`, then `source: "compact"` after `/compact`.
- `02`: the status line's input, with `context_window` (`used_percentage`, `remaining_percentage`,
  `context_window_size`, token totals), `cost` and lines added and removed.
- `03`–`05`: `FileChanged` for matcher files: `change`, `unlink`, and `add` for a file created after
  the session started. A `watchPaths` file outside the cwd fired nothing.
- `06`: `PreCompact` with `trigger: "manual"`.
`asyncRewake` is not a payload: the probe saw an idle session take a new turn when a background hook
exited 2.

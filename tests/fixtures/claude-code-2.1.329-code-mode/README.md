# Claude Code 2.1.329 hook payloads in Code Mode

Stdin of three hook events in an interactive Code Mode session
(`CLAUDE_CODE_EXPERIMENTAL_CODE_MODE=1`, auto mode) in a throwaway repository, redacted (no
machine paths or session ids). Recorded on 2026-11-20 for G808 and G810.

What the payloads show:

- A step that runs code ends with one `PostToolBatch` whose `tool_calls` hold the `REPL` call
  alone (its `code`, and `tool_response` "(dispatched #N)"): it fires when the code is dispatched.
  The calls the code makes are not in it.
- Each call the code makes reaches its own `PreToolUse` and `PostToolUse` under its usual tool
  name (`Bash`); the `REPL` call has a `PostToolUse` of its own.
- A `PreToolUse` hook's `additionalContext` on a call the code makes reaches the model, as an
  attachment of the code's step, while the code runs (seen in the session's transcript).

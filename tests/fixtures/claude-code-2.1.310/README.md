# Claude Code 2.1.310 hook payloads: a session that needs a person

Stdin of the hook events a session fires when it needs a person, in throwaway repositories, redacted (no machine
paths, session ids or prompt ids; `<redacted>` in their place, also inside text). Recorded on 2026-10-25 with
Claude Code 2.1.310 and Haiku, interactive in tmux unless said otherwise. Each payload is as Claude Code wrote it,
pretty-printed with one-space indents and only the redactions changed. Files named `hook-reply-*` are the probe
hook's own stdout, not Claude Code's. Input for recording why a session waits and for answering it from elsewhere
(R16 before any behaviour).

| Files | What they show |
|---|---|
| 01–04 | `AskUserQuestion` answered by a `PreToolUse` hook: `permissionDecision: "allow"` with `updatedInput` repeating `questions` and adding `answers` (the last option's label). No dialog opened. The terminal showed "User answered Claude's questions: → Blue", `PostToolUse` carries the answer in `tool_input.answers` and `tool_response.answers`, and the agent's reply (`Stop`'s `last_assistant_message`) was "Blue". |
| 05–08 | The same with an answer that is not an option label ("Green, a little darker than the logo"): passed through as written, and the agent repeated it. A free-text answer goes in the same `answers` string. |
| 09–11 | `ExitPlanMode` in plan mode: `tool_input` has `plan` (Markdown) and `planFilePath`; the hook's `allow` with `updatedInput` (the input echoed) approved it, and the agent left plan mode and carried the plan out. `tool_response` has `plan`, `filePath` and `isAgent`. |
| 12 | `Notification` for a permission prompt (manual mode, a Bash command): `notification_type: "permission_prompt"`, `message: "Claude needs your permission"`, no `title`. |
| 13–14 | A question asked in words: `Stop`'s `last_assistant_message` holds it ("Would you like a soft navy or a muted slate grey…"); about a minute later `Notification` fires with `notification_type: "idle_prompt"`, `message: "Claude is waiting for your input"`. |
| 15 | The input of a `Stop` hook set with `asyncRewake: true`: the ordinary `Stop` payload. |
| 16 | That hook exited 2 after 20 s with one line on stderr and one on stdout. The session woke. `woken_with` is the transcript's entry; `model_received` is what the model was sent: the stderr line (not stdout) as "Stop hook blocking error from command "Stop": …", inside a notice that says "NOT USER INPUT … Do NOT interpret this as user acknowledgement, confirmation, or response to any pending question". `assistant_after`: the agent did not take the line as the person's answer. |
| 17 | The same wake, telling the agent to ask its question again with `AskUserQuestion`. The agent declined: it kept to the person's "stop and wait", and said it would not treat the hook's message as their reply. |
| 18–23 | `ExitPlanMode` refused with words (recorded 2026-10-25, plan mode): the hook replied `permissionDecision: "deny"` with `permissionDecisionReason: "No: keep notes.txt as it is, and put the word hello in docs/hello.txt instead."` (19). The agent received it as a tool result with `is_error: true`: "PreToolUse:ExitPlanMode hook error: No: …" (23). It rewrote the plan on that feedback ("Per feedback, `notes.txt` stays untouched…", 20) and presented it again; the second plan was allowed (21–22). So a refusal with a person's words revises the plan. |

Also observed, not in a file:

- Headless (`claude -p`, 2.1.310): `AskUserQuestion` is not among the agent's tools. Asked to use it, the agent said
  it was not in its tool list and asked in words instead. A question from a headless session is always in words.
- While a `PreToolUse` hook on `AskUserQuestion` waits, the terminal shows only "running PreToolUse hook · 8s"; the
  question itself is not shown until the hook answers or ends.
- A `Stop` hook with `asyncRewake` fires again at every `Stop`, the woken turn's included, so each wake woke the session
  again: three turns in under two minutes until the session was closed.
- A `Notification` for a turn the person interrupts (Escape on the permission prompt) was not followed by `Stop` or
  `idle_prompt` within 100 s.

What follows for a design, in short: the one way a person's answer reaches an agent as theirs is `AskUserQuestion`
(or `ExitPlanMode`) answered by a `PreToolUse` hook, in an interactive session. A wake can tell an idle session that
something happened, but not deliver an answer, and a question asked in words can be shown, not answered from
elsewhere.

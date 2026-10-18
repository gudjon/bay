# Claude Code 2.1.302 hook payloads

Stdin of every hook event in three headless runs in throwaway repositories, redacted (no
machine paths or session ids). Recorded by the bay-researcher session on 2026-10-18; the
method and findings are research chapter 23 (`23-claude-code-hook-probe.md`).
Input for the increment-2 hook adapter (`DESIGN.md` § 11, § 11a; G23).

What the payloads show:

- `PostToolBatch` carries `tool_calls: [{tool_name, tool_input, tool_use_id, tool_response}]`
  and fires for subagents with `agent_id` and `agent_type`; a subagent keeps the parent's
  `session_id`.
- The shell tool reports no changed files (`echo b > b.txt` appears only in the command), so
  the adapter reads the view's own Git status; it never trusts the payload for writes.
- A denied tool call (`*-denied.json`) fires `PreToolUse`, then no `PostToolUse` and no denial
  event; the denial is only text in `PostToolBatch`. bay does not parse denials.

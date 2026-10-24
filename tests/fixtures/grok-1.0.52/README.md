# Grok Build 1.0.52 hook payloads, as a guest of a Claude Code repository

Recorded 2026-10-24 by bay-researcher on a throwaway repository with only `.claude/settings.json` (command hooks that
write their stdin to a file), `GROK_FOLDER_TRUST=0` for one process, `grok -p … --always-approve --cwd <repo>`.
Grok runs the project's Claude hooks: its payload carries Claude's snake_case keys beside its own camelCase ones
(`hookEventName`, `workspaceRoot`, …), but `tool_name` is Grok's own: `run_terminal_command` (the shell, with
`tool_input.command`) and `write` (with `tool_input.file_path`). Input for bay's guest dialect (BACKLOG 52 H4,
`src/agent_hook/guest.rs`).

Redacted on import: paths (the probe root is /work, the home dir /home/user, as recorded), and session, tool-call and
prompt ids (`session-<n>`, `toolu-<n>`, `prompt-<n>`), also where a path embeds them.

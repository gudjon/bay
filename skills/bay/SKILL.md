---
name: bay
description: "Merge changes into main with bay: open a change, submit it once, let bay merge the exact checked result, and act on refusals. Use it when the repository merges with bay (it has .bay/config.toml on main, or the user says so), when several agents work in one codebase, or when the user asks to set bay up. Read the bay.result/1 document on stdout. Unknown is not success."
allowed-tools:
  - Bash(bay:*)
  - Read
---

# bay

bay merges coding agents' changes into main, so several agents can work in one codebase without stale
branches, colliding merges or a broken main. You open a change, edit in its view, and submit it once. A
runner checks the exact candidate and merges it, even after you leave. A few field and event names still
say `trunk` for main.
Main is the one branch bay serves, whatever its name (`bay install --main dev`). Create no branches: a
change is bay's own record, and its view is a folder bay makes. Branches after main, as in
`dev → staging → production`, are `[[target]]`s; work that must never merge goes in a `[[lane]]`.
Docs: https://getbay.io/docs (each page also reads as markdown: add `.md` to its URL).

## Before you start

1. `bay --version`. If `bay` is missing and the user wants bay, install it:
   `curl -fsSL https://getbay.io/install.sh | sh`. It verifies a signed release and needs no root.
2. `bay status` in the repository. `CONFIG_MISSING` means bay is not set up here: see "Set up a
   repository" below, or ask the user.
3. `bay doctor` names anything missing on this machine, each with its fix.

## The daily loop

| When | Run |
| --- | --- |
| Open one change, or a follow-up after the intent's change merged | `bay start <intent> --paths <paths> [--input <kind>:<name>]` |
| Work | Edit only inside the view that `bay start` prints. |
| Submit the change | `bay merge` (returns at once; a runner checks and merges it) |
| Wait for the outcome here | `bay merge --wait` (`--wait=<seconds>`: watch that long, then return; the run goes on) |
| Submit a change from outside its view | `bay merge <change>` |
| Pick a change up again: bring in newer main and see what moved | `bay start <change>` |
| Fix a change that already merged | `bay start <intent> --fixes <change>` (the merge names it in a `Fixes:` trailer) |
| Try work that must never merge, in a sandbox lane the config declares | `bay start <intent> --paths <paths> --lane <lane>` (`bay merge` runs its checks; it never merges) |
| Take in a branch made outside bay (a bot's or a person's) as one change | `bay start <intent> --paths <paths> --from <branch>` (the remote's tip when the remote has the branch; `--paths` must cover every file it changes; then `bay merge`) |
| Merge a change only after another change merged | `bay start <intent> --paths <paths> --after <change>` (repeatable; submit it now, it merges once each one merged) |
| Save edits made in the main checkout (non-developers) | `bay save <intent>` |
| See the changes waiting for this machine's approver: candidate, paths, diff summary | `bay waiting` (`detail.waiting`) |
| Approve one, or say no, when the approver asks you to | `bay approve <change> [--why <text>]`, `bay reject <change> [--why <text>]` |
| Waive one failed check marked `waivable`, when the approver asks you to | `bay approve <change> --waive <check> --why <text>` (that candidate only; never a pass) |
| Add one key to the key registry, when the user sets up keys | `bay enrol <class> <name> <key.pub>` (class `registry`, `approver`, `reviewer`, `producer`, `executor` or `agent`; signed by `bay.registryKey`) |
| Give the change up, so another agent may adopt it | `bay abandon [<change>]` (default: the view's change) |
| Start over after abandoning, with a fresh change | `bay start <intent> --new` |
| Read state, and main commits that did not merge through bay (`unattributed`; `by_writer` counts them per committer) | `bay status` |
| See the flow: live changes by actor and their limit, merges this week, cycle time, queue wait and check time (p50, p90), the oldest live change, what waits for a yes | `bay status` (`detail.flow`) |
| See each promotion target (`staging`): its tip, `drift`, `lag`, checks, freeze and the `promotable` commit | `bay status` (`detail.targets`) |
| See whether main is held, by whom (`source`: `window` or `plugin`), why and until when | `bay status` (`detail.hold`) |
| Reverse a merged change with a new, checked change | `bay undo <change> [--wait[=<seconds>]]` |

After a merge, a plain `bay start <intent>` opens the next change. `bay start <change>` picks up an
existing change.
`<intent>` is the card, issue or ticket id. `--paths` lists every path the change reads and writes:
the view holds only those. `bay start` lists in `detail.overlaps` the live changes whose
declared paths overlap yours (change, intent, actor, paths). It does not refuse; talk to them, or
narrow your paths. `bay help` shows every command and flag on one page, with what each one writes and
never writes (read it before you hesitate to run one), and `bay <command> --help`
says more about one. `bay help --json` is the same list for programs, with every reason code and
the JSON Schema of `bay.result/1`.
It also lists each agent product's hook moments and what each can block.

## Read the result

- Always pass `--json`. stdout is then one `bay.result/1` JSON document, and text on stderr is
  not the result. Without it, a terminal shows a person the plain sentence instead. bay never
  prompts.
- `say` is one plain sentence for a person who does not know Git: what happened and the next
  step. Tell it to such a user as it stands.
- `decision`: `merged`, `queued` and `ok` exit 0; `blocked` exits 10; `failed` exits 20; a usage
  error exits 2.
- `queued` means the merge has not happened yet; only `merged` means it has. `waiting_for` says
  what it waits for. Do not report it as merged.
- While a change waits in the queue, `bay status <change>` may show `operation.conflict`: main
  moved into a `MERGE_CONFLICT` on the listed paths. It keeps its place; refresh and repair now.
  The agent hook also tells your session once.
- A `landed_on_your_paths` notice: another change merged on paths you declared. Refresh, re-read
  the named paths, then go on.
- `bay status` with `detail.warnings` naming `no_live_executor`: work is queued and no runner has
  been seen recently. Tell the user a runner must run (`bay runner run`, or
  `bay runner install` to keep one running). The work stays queued. The agent hook says the
  same once, at session start or a prompt: "bay: no runner is running for this repository".
- `bay status` with `detail.warnings` naming `trunk_not_linear`: a merge pushed outside bay put
  merges off main's first-parent line. bay still finds them. Tell the user to require linear
  history on the host.
- A `blocked` or `failed` result names a `reason`, the actions still `allowed`, and `retry_when`:
  the condition under which a retry can succeed. bay refuses an unchanged retry, except for a few
  transient reasons that it retries itself.
- Unknown is not success. A missing, unreadable or unexpected result is a failure to report.

`allowed` uses four words. What each one means:

| Action | Do |
| --- | --- |
| `read` | Read what `detail` names, such as the new version of an input. |
| `refresh` | `bay start <change>`: brings in newer main and reports what moved. |
| `repair` | Edit in the view, then `bay merge` again. |
| `wait` | Nothing yet. `bay merge --wait`, or `bay status` later. |

Every refusal carries `next_step`: do that. `bay help --json` lists every reason with its next step.

A few reasons need more than their next step:

| Reason | Also |
| --- | --- |
| `CHECK_FAILED` | A check with `baseline = "base"` names only new failures; pre-existing ones appear as `baseline_failures` in `coverage_gaps`: report them, never as passed. A check with `waivable = true` may instead get an approver's waiver (`bay waiting` lists it); then `bay merge --wait`. The waiver appears in `coverage_gaps` with the real verdict: report it, never as passed. |
| `CHECK_FLAKY` | A merge may name one flaky in `coverage_gaps`: report it, never as passed. |
| `WIP_LIMIT` | From `bay start`: you hold the most live changes (`detail.live_changes`). From `bay merge`: the review queue is full (`detail.review`: `waiting`, `max`, `oldest`); do `detail.next`. |
| `POLICY_INVALID` | Every merge is judged by the policy on main, so a broken one blocks them all, its own repair too. The repair merges when signed by the registry key: tell the user; on the machine that holds it, `git config bay.agentKey <registry key>` for that one `bay merge`, then set it back. Never do this unasked. On a main from `bay host init` this path is not yet proven: tell the user so. |
| `APPROVAL_PENDING` | Tell the user who can approve (`detail.approval.checks[].approvers`). An approval holds while its paths are unchanged (`checks[].set_aside` names the ones that changed). Never approve for them. |

## When the person is not a developer

Many people who edit with you have never used Git. Keep bay invisible to them:
- **Speak their four words:** save, waiting, approve, undo.
  - "Save my work": `bay save <what it is>`.
  - "What's waiting for me?": `bay waiting`.
  - "Approve it": `bay approve <change>`. Their own Touch ID or key is the confirmation; never approve for them.
  - "Undo that": `bay undo <change>`.
- **Tell them `say`.** Every result carries `say`, one plain sentence. Read it to them as it is, then add at most
  one next step. Never say `branch`, `commit`, `merge`, `trunk`, `rebase`, `ref` or `view` to them.
- **Show before they approve.** Before `bay approve`, show what changes: the files and a short plain summary of the
  diff from `bay waiting`. Ask yes or no.
- **One place.** Everything happens in this conversation. Don't send them to a web page, a dashboard or a
  terminal command they would have to type themselves.

## Work with other agents

| When | Run |
| --- | --- |
| See every machine and session on the repository, who works on red paths (`on_red`), the open talks (`talks`; each session's `owes_talks`), and the runners with their age (`executors`; `stale`); each machine's last `bay doctor` verdict (`doctor`; `null` if it never ran) and the fleet's count (`ready`: green, red, never ran, lowest bay version) | `bay status --fleet` |
| See every head: each branch on the remote, classed `trunk`, `target`, `lane`, `landed`, `stale` or `unique`, and in `detail.stranded` the local branches no remote holds and worktrees whose folder is gone; deletes nothing | `bay status --heads [--stale-days <n>]` |
| Delete only the remote branches `landed` by `ancestor` or `patch`, each archived first and restorable from `.git/bay/pruned-heads.log`; a `[plugin.prune]` plugin runs after each delete (`detail.pruned[].on_prune`) | `bay prune [--stale-days <n>]` |
| See merges, events (`trunk_red`, `trunk_red_cleared`, `unattributed_landing`, `trunk_not_linear`, `target_moved`, `target_unpromotable`, `talk_arrived`, `talk_closed`, `wake`) and invalidations since you last looked | `bay status --since <cursor>` (pass the returned cursor next time) |
| Ask another agent for one change, decision or check, with a reply time | `bay talk <agent> "<what>" --kind delivery\|discovery\|reliability --follow-up <time>` |
| Answer a talk addressed to you | `bay talk --answer <id> --value <v>` |

Run `bay talk --help` for the fields of each kind. One talk asks for one thing.

- A talk read after its `--expires` is void: the notice prints `EXPIRED`, and it owes no reply.
- `bay merge` and `bay undo` list the open talks about that change in `detail.open_talks`. Read them;
  they never block the merge.
- With a `[plugin.wake] command`, bay wakes your session once for a talk to you, and once when a runner
  merges your change or blocks it on a step that is yours (`cause: "outcome"`). The plugin's answer
  is the event's `wake` in `bay status --since`. An outcome your own `bay merge --wait` returns wakes nothing.

## Set up a repository (only when the user asks)

1. Commit `.bay/config.toml` and `.bay/policy.toml` to main and push them. bay reads both only from
   main. https://getbay.io/docs/configuration lists every key. Name the agent product in
   `[agent] product`: `claude` or `codex`; `bay install` writes bay's hooks where that product
   reads them. Codex's hooks only sense (Codex runs a tool when a hook fails), so main is the
   guard there. To hold main (an incident, a
   migration), add a `[[hold]]` with `end` and `reason` (a `target` holds a promotion target instead), or a
   `[plugin.hold] command` that answers `{"held": true|false, "reason": "..."}` to `<command> status`.
   To act on the host when `bay prune` deletes a branch (close its pull request,
   say), add `[plugin.prune] command`: it runs as `<command> pruned <branch> <sha> <archive ref>`.
   A key a newer bay brought is a config error on an older bay, naming the key: run `bay update` there.
2. `.bay/policy.toml` lists the required checks. Each `[[check]]` needs `expected_minimum`. A
   repository that wants no checks says `unchecked = true`. Paths that need a person's approval
   get a `[[check]]` with `kind = "approval"` and `when_paths`, and no command; each approver is
   enrolled with `bay enrol approver <name> <key.pub>` and approves with `bay approve <change>`.
   It records a signed approval record of the exact candidate.
   A run check may say `waivable = true` (never an approval check): an enrolled approver may then
   waive it on one candidate with `bay approve <change> --waive <check> --why <text>`. A waiver
   is never a pass; the merge names it in `coverage_gaps`. Without a registry no waiver counts.
   A small team with no runner host sets `[policy] signing = "approvals"`: only approvals are
   signed, and no machine needs an `executor`, `producer` or `agent` key. Each clone takes its own
   producer name (`local-<id>` in its git config); set `bay.producerName` only to choose one.
   There, and with no registry, evidence reuse and batching stay off unless the policy also says
   `[policy] reuse_unsigned = true`. Results still name their trust: `unsigned` or `approvals`.
   To keep changes small, add `[limits]` with `max_file_mib`, `max_files` or `max_added_lines`
   (positive integers; each optional). To cap the review queue, add `max_waiting_approvals`:
   with that many changes waiting for a yes, a change that needs one more is refused `WIP_LIMIT`.
3. Before you commit a `[[check]]` command or a plugin (`[plugin.wake]`, `[plugin.notify]`),
   prove it: `bay plugin test check|wake|notify -- <command>`. It runs bay's contract cases the
   way bay runs it, changes nothing, and exits 0 only when every case passes.
   `detail.cases` names each failing case and why.
4. With full signing, each agent's machine sets `bay.agentKey`, and the agent runs with `BAY_ACTOR=<its enrolled
   name>`: the host refuses a change signed by one agent for another actor (the default actor is Git's user name).
5. In each clone: `bay install --main <name> --remote <name>`. It puts this skill in every Claude
   config directory it finds on the machine. Name any other one with
   `git config --global --add bay.claudeConfigDirs <dir>` (each value is one config directory).
6. Run a runner: `bay runner run`, under a supervisor. `bay runner install` writes one for
   this repository (a launchd agent on macOS, a systemd user unit on Linux) and prints the one
   command that starts it; it starts nothing. Tell the user that command. `--print` only shows it.
7. A check that must read a directory outside its tree (a package store) needs it named on the
   machine that runs checks: `git config --global --add bay.checkReadPath ~/<dir>`. It is read-only.

## What bay guards, and when

- **Its own merges, always:** what merges through bay is exactly what was checked.
- **An agent's Git commands, only with the hook:** `[agent] product = "claude"` in `.bay/config.toml`, then
  `bay install`. The hook then refuses a push to main, a force push, `reset --hard` and the like. Without it,
  `bay doctor`'s `hooks` row reads `off` and an agent can push main around bay.
- **Everything that reaches main, only on a bay-hosted main** (`bay host init`). On a forge such as GitHub, a merge
  made in its web page or API, and a script's push, never pass a local hook; protect main there so that only the
  runner may push it.
- bay sees what went around it: `bay status` counts it in `detail.unattributed` (per committer in `by_writer`), and `bay doctor` is red when main
  bay merges into also holds such merges. Tell the user; do not work around it.

## Roll bay's hooks out to many machines

The order, learned in a fleet rollout:
1. On every machine: `bay update`, then `bay install --machine`. It writes only the bay program and its guide, in
   every Claude config directory it finds (`bay.claudeConfigDirs` names others). It never writes hook entries, a
   repository's settings or its Git config, so it is safe to run on a busy machine.
2. Put `[agent] product = "claude"` in `.bay/config.toml` on main. Each machine's `bay doctor` now reads hooks red
   until step 3 merges: that is expected, not a fault.
3. In one clone, run `bay install`, and merge the change that commits `.claude/settings.json` with bay's entries.
   Every machine gets the hooks as it takes that change. A machine without bay is unaffected: each entry first
   checks that bay is present.
4. Run `bay doctor` on each machine after it takes the change. `bay status --fleet` then shows every machine's
   verdict in one place (`ready`); fix each red machine it names, and do not guess.

## Set up bay for a team (agent-led)

When the user asks to set bay up for a small team, do each step for them. Tell them `say` after each.

1. On each person's machine: install bay (see "Before you start"), then `bay install` in their copy.
2. Put `.bay/config.toml` and `.bay/policy.toml` on main and push them. The policy says
   `[policy] signing = "approvals"`, with an approval `[[check]]` for the paths that need a yes.
3. On the machine that keeps the registry key: `git config bay.registryKey <private key path>`.
   Then `bay enrol registry <name> <key.pub>`. The first key is always this registry key.
4. For each person who approves: `bay enrol approver <name> <their key.pub>`. Ask only for the
   public key, the `.pub` file. A name is a short lower-case word.
5. On each approver's machine: `git config bay.approverName <name>` and
   `git config bay.approverKey <their private key path>`.
6. `bay doctor` on each machine. It names anything missing, with its fix, and publishes its verdict, so
   `bay status` then `bay status --fleet` on any one machine shows every machine's (`ready`).

`bay enrol` refuses a key that is already enrolled, and says why. Nothing is written then.

## Never

- Never push main, merge into main, or move main yourself. bay merges.
- Never push a target such as `staging` (`[[target]]` in the config). bay's hook refuses it.
- Never run `git reset --hard`, `git clean -f`, `git branch -D`, `git checkout .`, `git restore .`, a force push,
  or a push of `--all` or `--mirror`. bay's hook refuses them, also inside `bash -c`.
- Never merge, cherry-pick, check out or restore from a lane's refs (`[[lane]]` in the config) in a
  view that is not of that lane. bay's hook refuses those commands, and `bay merge` refuses lane commits.
- Never approve on your own, or ask for an approver's key. Only the person approves: run
  `bay approve` only when they ask, on their machine, where their key signs it.
- Never grant a key, widen the policy, quarantine a check, or set `allow_flaky` to get a change through.
  Run `bay enrol` only when the user sets up keys.
- Never run `bay install` to merge work. It sets up a machine or a clone.
- Never report a change as merged until the result says `merged`.

## This skill

`bay install` copies the copy of this skill that matches the installed bay. `bay doctor` reports
`SKILL_MISMATCH` when they differ; run `bay install` again. Source:
https://github.com/gudjon/bay/blob/main/skills/bay/SKILL.md

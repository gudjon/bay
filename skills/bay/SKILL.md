---
name: bay
description: "Merge changes into main with bay (its commands call main trunk and a checked merge a landing): open a change, submit it once, let bay merge the exact checked result, and act on refusals. Use it when the repository merges with bay (it has .bay/config.toml on main, or the user says so), when several agents work in one codebase, or when the user asks to set bay up. Read the bay.result/1 document on stdout. Unknown is not success."
allowed-tools:
  - Bash(bay:*)
  - Read
---

# bay

bay merges coding agents' changes into main, so several agents can work in one codebase without stale
branches, colliding merges or a broken main. You open a change, edit in its view, and submit it once. An
executor checks the exact candidate and merges it, even after you leave. bay's commands and results call
main `trunk` and a checked merge a landing (`bay land`, `landed`); this skill uses the plain words.
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
| Open one change | `bay start <intent> --paths <paths> [--input <kind>:<name>]` |
| Work | Edit only inside the view that `bay start` prints. |
| Submit the change | `bay land` (returns at once; an executor checks and merges it) |
| Wait for the outcome here | `bay land --wait` |
| Bring in newer main and see what moved | `bay start --resume <change>` |
| A follow-up after your change merged | `bay start <intent> --new` |
| Fix a change that already merged | `bay start <intent> --fixes <change>` (the merge names it in a `Fixes:` trailer) |
| Try work that must never merge, in a sandbox lane the config declares | `bay start <intent> --paths <paths> --lane <lane>` (`bay land` runs its checks; it never merges) |
| Give the change up, so another agent may adopt it | `bay land --abandon` |
| Read state, and main commits that did not merge through bay (`unattributed`) | `bay status` |
| See each promotion target (`staging`): its tip, `drift`, `lag`, checks, freeze and the `promotable` commit | `bay status` (`detail.targets`) |
| Reverse a merged change with a new, checked change | `bay undo <change>` |

`<intent>` is the card, issue or ticket id. `--paths` lists every path the change reads and writes:
the view holds only those. `bay start` lists in `detail.overlaps` the live changes whose
declared paths overlap yours (change, intent, actor, paths). It does not refuse; talk to them, or
narrow your paths. Run `bay <command> --help` for every flag. `bay help --json` lists every
command, flag and reason code, with the JSON Schema of `bay.result/1`.

## Read the result

- stdout is one `bay.result/1` JSON document. Text on stderr is not the result. Add `--json` to
  print the document alone. bay never prompts.
- `decision`: `landed`, `queued` and `ok` exit 0; `blocked` exits 10; `failed` exits 20; a usage
  error exits 2.
- `queued` with `"published": false` means the merge has not happened yet. `waiting_for` says
  what it waits for. Do not report it as merged.
- While a change waits in the queue, `bay status <change>` may show `operation.conflict`: main
  moved into a `MERGE_CONFLICT` on the listed paths. It keeps its place; refresh and repair now.
  The agent hook also tells your session once.
- A `landed_on_your_paths` notice: another change merged on paths you declared. Refresh, re-read
  the named paths, then go on.
- `bay status` with `detail.warnings` naming `no_live_executor`: work is queued and no executor has
  been seen recently. Tell the user an executor must run (`bay executor run`). The work stays queued.
- A `blocked` or `failed` result names a `reason`, the actions still `allowed`, and `retry_when`:
  the condition under which a retry can succeed. bay refuses an unchanged retry, except for a few
  transient reasons that it retries itself.
- Unknown is not success. A missing, unreadable or unexpected result is a failure to report.

What each `allowed` action means:

| Action | Do |
| --- | --- |
| `read` | Read what `detail` names, such as the new version of an input. |
| `refresh` | `bay start --resume <change>`: brings in newer main and reports what moved. |
| `repair` | Edit in the view, then `bay land` again. |
| `wait` | Nothing yet. `bay land --wait`, or `bay status` later. |

Common reasons:

| Reason | Do |
| --- | --- |
| `TRUNK_MOVED` | Nothing. bay retries once more, then joins the queue. |
| `INPUT_CHANGED` | Read the new version, refresh, repair, then `bay land` again. |
| `INPUT_UNOBSERVED` | Read the required version, then refresh. |
| `MERGE_CONFLICT` | Refresh, resolve the listed paths in the view, then `bay land` again. |
| `CHECK_FAILED` | Repair the change, then `bay land` again. |
| `CHECK_UNKNOWN` | Wait for a result. Never treat it as a pass. |
| `CHECK_FLAKY` | Tell the user: the check gives two answers for one tree. A landing may name one flaky in `coverage_gaps`: report it, never as passed. |
| `TRUNK_RED` | Main is red on these paths. Wait, or fix the owner's change: `bay start --fixes <change>`. |
| `PROMOTION_REFUSED` | The host refused a push to a target such as `staging`. Push the commit `bay status` names promotable, or wait. |
| `APPROVAL_PENDING` | These paths need a person's yes. Tell the user who can approve (`detail.approval.checks[].approvers`) and the candidate commit to approve (`detail.approval.commit`). Wait, then `bay land --wait` again. |
| `COVERAGE_UNKNOWN` | Keep editing; `bay land` when coordination data is reachable. |
| `LANE_NEVER_LANDS` | A lane change: read the check verdicts in `detail`. It never merges. |
| `LANE_LEAK` | The view holds a lane's commits (`detail.leaks`). Redo the work without them, or use `--lane`. |
| `ACTOR_UNPROVEN` | Policy requires proof of who signs, and actors share this OS account. Tell the user: a hardware agent key, or one OS account per actor. |
| `WIP_LIMIT` | You hold the most live changes policy allows (`detail.live_changes`). Land or abandon one, then start again. |
| `DISK_LOW` | This box is below its disk floor (`detail.free_mib`, `detail.floor_mib`). Tell the user: free space, or lower `bay.minFreeMiB`. |
| `CONFIG_MISSING` / `POLICY_MISSING` | The repository is not set up. Tell the user, or set it up if asked. |

Every code: https://getbay.io/docs/results

## Work with other agents

| When | Run |
| --- | --- |
| See every machine and session on the repository, who works on red paths (`on_red`), the open talks (`talks`; each session's `owes_talks`), and the executors with their age (`executors`; `stale`) | `bay status --fleet` |
| See merges, events (`trunk_red`, `trunk_red_cleared`, `unattributed_landing`, `target_moved`, `target_unpromotable`, `talk_arrived`, `talk_closed`) and invalidations since you last looked | `bay watch --since <cursor>` (pass the returned cursor next time) |
| Ask another agent for one change, decision or check, with a reply time | `bay talk <agent> "<what>" --kind delivery\|discovery\|reliability --follow-up <time>` |
| Answer a talk addressed to you | `bay talk --answer <id> --value <v>` |

Run `bay talk --help` for the fields of each kind. One talk asks for one thing.

- A talk read after its `--expires` is void: the notice prints `EXPIRED`, and it owes no reply.
- `bay land` and `bay undo` list the open talks about that change in `detail.open_talks`. Read them;
  they never block the merge.

## Set up a repository (only when the user asks)

1. Commit `.bay/config.toml` and `.bay/policy.toml` to main and push them. bay reads both only from
   main. https://getbay.io/docs/configuration lists every key.
2. `.bay/policy.toml` lists the required checks. Each `[[check]]` needs `expected_minimum`. A
   repository that wants no checks says `unchecked = true`. Paths that need a person's approval
   get a `[[check]]` with `kind = "approval"` and `when_paths`, and no command; each approver is
   enrolled in the registry's `approver` file and records with
   `bay evidence record --kind approval --check <id> --verdict pass --commit <candidate>`.
3. Before you commit an adapter (a `[[check]]` command, `[wake] command` or `[notify] command`),
   prove it: `bay adapter test check|wake|notify -- <command>`. It runs bay's contract cases the
   way bay runs that adapter, changes nothing, and exits 0 only when every case passes.
   `detail.cases` names each failing case and why.
4. In each clone: `bay install --trunk <name> --remote <name>`.
5. Run an executor: `bay executor run`, under a supervisor.

## Never

- Never push main, merge into main, or move main yourself. bay merges.
- Never push a target such as `staging` (`[[target]]` in the config). bay's hook refuses it.
- Never run `git reset --hard`, `git clean -f`, `git branch -D`, `git checkout .`, `git restore .`, a force push,
  or a push of `--all` or `--mirror`. bay's hook refuses them, also inside `bash -c`.
- Never merge, cherry-pick, check out or restore from a lane's refs (`[[lane]]` in the config) in a
  view that is not of that lane. bay's hook refuses those commands, and `bay land` refuses lane commits.
- Never grant a key, widen the policy, or quarantine a check to get a change through.
- Never record an approval yourself, or ask for an approver's key. Only the person approves.
- Never grant a key, widen the policy, quarantine a check, or set `allow_flaky` to get a change through.
- Never run `bay install` to merge work. It sets up a machine or a clone.
- Never report a change as merged until the result says `landed`.

## This skill

`bay install` copies the copy of this skill that matches the installed bay. `bay doctor` reports
`SKILL_MISMATCH` when they differ; run `bay install` again. Source:
https://github.com/gudjon/bay/blob/main/skills/bay/SKILL.md

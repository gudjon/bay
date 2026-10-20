---
name: bay
description: "Land changes on a Git repository's trunk with bay: open a change, land the exact checked result, read refusals and act on them. Use it when the repository lands changes with bay (it has .bay/config.toml on trunk, or the user says so), when several agents share one repository, or when the user asks to set bay up. Read the bay.result/1 document on stdout. Unknown is not success."
allowed-tools:
  - Bash(bay:*)
  - Read
---

# bay

bay lands changes on trunk for coding agents that share one Git repository. You open a change, edit
in its view, and submit it once. An executor checks the exact candidate and lands it, even after you
leave. Docs: https://getbay.io/docs (each page also reads as markdown: add `.md` to its URL).

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
| Submit the change | `bay land` (returns at once; an executor checks and lands it) |
| Wait for the outcome here | `bay land --wait` |
| Bring in newer trunk and see what moved | `bay start --resume <change>` |
| A follow-up after your change landed | `bay start <intent> --new` |
| Fix a change that already landed | `bay start <intent> --fixes <change>` (the landing names it in a `Fixes:` trailer) |
| Give the change up, so another agent may adopt it | `bay land --abandon` |
| Read state, and trunk commits that did not land through bay (`unattributed`) | `bay status` |
| Reverse a landed change with a new, checked change | `bay undo <change>` |

`<intent>` is the card, issue or ticket id. `--paths` lists every path the change reads and writes:
the view holds only those. Run `bay <command> --help` for every flag.

## Read the result

- stdout is one `bay.result/1` JSON document. Text on stderr is not the result. Add `--json` to
  print the document alone. bay never prompts.
- `decision`: `landed`, `queued` and `ok` exit 0; `blocked` exits 10; `failed` exits 20; a usage
  error exits 2.
- `queued` with `"published": false` means the landing has not happened yet. `waiting_for` says
  what it waits for. Do not report it as landed.
- A `blocked` or `failed` result names a `reason`, the actions still `allowed`, and `retry_when`:
  the condition under which a retry can succeed. bay refuses an unchanged retry, except for a few
  transient reasons that it retries itself.
- Unknown is not success. A missing, unreadable or unexpected result is a failure to report.

What each `allowed` action means:

| Action | Do |
| --- | --- |
| `read` | Read what `detail` names, such as the new version of an input. |
| `refresh` | `bay start --resume <change>`: brings in newer trunk and reports what moved. |
| `repair` | Edit in the view, then `bay land` again. |
| `wait` | Nothing yet. `bay land --wait`, or `bay status` later. |

Common reasons:

| Reason | Do |
| --- | --- |
| `TRUNK_MOVED` | Nothing. bay retries once more, then joins the queue. |
| `INPUT_CHANGED` | Read the new version, refresh, repair, then land again. |
| `INPUT_UNOBSERVED` | Read the required version, then refresh. |
| `MERGE_CONFLICT` | Refresh, resolve the listed paths in the view, then land again. |
| `CHECK_FAILED` | Repair the change, then land again. |
| `CHECK_UNKNOWN` | Wait for a result. Never treat it as a pass. |
| `CHECK_FLAKY` | Tell the user: the check gives two answers for one tree. |
| `TRUNK_RED` | Trunk is red on these paths. Wait, or fix the owner's change: `bay start --fixes <change>`. |
| `COVERAGE_UNKNOWN` | Keep editing; land when coordination data is reachable. |
| `CONFIG_MISSING` / `POLICY_MISSING` | The repository is not set up. Tell the user, or set it up if asked. |

Every code: https://getbay.io/docs/results

## Work with other agents

| When | Run |
| --- | --- |
| See every machine and session on the repository, and who works on red paths (`on_red`) | `bay status --fleet` |
| See landings, events (`trunk_red`, `trunk_red_cleared`, `unattributed_landing`) and invalidations since you last looked | `bay watch --since <cursor>` (pass the returned cursor next time) |
| Ask another agent for one change, decision or check, with a reply time | `bay talk <agent> "<what>" --kind delivery\|discovery\|reliability --follow-up <time>` |
| Answer a talk addressed to you | `bay talk --answer <id> --value <v>` |

Run `bay talk --help` for the fields of each kind. One talk asks for one thing.

- A talk read after its `--expires` is void: the notice prints `EXPIRED`, and it owes no reply.
- `bay land` and `bay undo` list the open talks about that change in `detail.open_talks`. Read them;
  they never block the landing.

## Set up a repository (only when the user asks)

1. Commit `.bay/config.toml` and `.bay/policy.toml` to trunk and push them. bay reads both only from
   trunk. https://getbay.io/docs/configuration lists every key.
2. `.bay/policy.toml` lists the required checks. Each `[[check]]` needs `expected_minimum`. A
   repository that wants no checks says `unchecked = true`.
3. In each clone: `bay install --trunk <name> --remote <name>`.
4. Run an executor: `bay executor run`, under a supervisor.

## Never

- Never push trunk, merge into trunk, or move trunk yourself. bay lands.
- Never run `git reset --hard`, `git clean -f`, `git branch -D`, `git checkout .`, `git restore .`, a force push,
  or a push of `--all` or `--mirror`. bay's hook refuses them, also inside `bash -c`.
- Never grant a key, widen the policy, or quarantine a check to get a change through.
- Never run `bay install` to land work. It sets up a machine or a clone.
- Never report a change as landed until the result says `landed`.

## This skill

`bay install` copies the copy of this skill that matches the installed bay. `bay doctor` reports
`SKILL_MISMATCH` when they differ; run `bay install` again. Source:
https://github.com/gudjon/bay/blob/main/skills/bay/SKILL.md

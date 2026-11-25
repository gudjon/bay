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
runner checks the exact candidate and merges it, even after you leave. Main is the one branch bay serves,
whatever its name: the remote's default branch. Create no branches: a
change is bay's own record, and its view is a folder bay makes. Branches after main, as in
`dev → staging → production`, are `[[target]]`s; work that must never merge goes in a `[[lane]]`.
Docs: https://getbay.io/docs (each page also reads as markdown: add `.md` to its URL).

## Before you start

1. `bay --version`. If `bay` is missing and the user wants bay, install it:
   `curl -fsSL https://getbay.io/install.sh | sh`. It verifies a signed release and needs no root.
2. `bay status` in the repository. `CONFIG_MISSING` means bay is not set up here: read `setup.md` in
   this skill, or ask the user.
3. `bay doctor` names anything missing on this machine, each with its fix.

## The daily loop

| When | Run |
| --- | --- |
| Open one change, or a follow-up after the intent's change merged | `bay start <intent> --paths <paths> [--input <kind>:<name>]` |
| Ask what a change to some paths would meet, the checks and the live changes on them, starting nothing | `bay start <intent> --paths <paths> --dry-run` |
| Work | Edit only inside the view that `bay start` prints. |
| Submit the change | `bay merge` (returns at once; a runner checks and merges it) |
| Wait for the outcome here | `bay merge --wait=<seconds>` (watch that long, then return; the run goes on). A bare `--wait` returns `queued` while other changes are ahead |
| Submit a change from outside its view | `bay merge <change>` |
| Pick a change up again: bring in newer main and see what moved | `bay start <change>`. When it remade an unfinished view, `detail.unsaved_kept` names where its unsaved files were kept: copy them back, then `bay merge`. A view of a change that ended a retention window ago is freed with its unsaved work kept: `git checkout refs/bay/view-kept/<change> -- <path>` brings a file back, and its loose ignored files such as `.env` are under `.git/bay/view-kept/<change>/` |
| Fix a change that already merged | `bay start <intent> --fixes <change>` (the merge names it in a `Fixes:` trailer) |
| Merge a change only after another change merged | `bay start <intent> --paths <paths> --after <change>` (repeatable; submit it now, it merges once each one merged) |
| Give the change up, so another agent may adopt it | `bay abandon [<change>] [--why "<reason>"]` (default: the view's change; say why, e.g. where the work merged) |
| Start over after abandoning, with a fresh change | `bay start <intent> --new` |
| Take up a change you abandoned again, with its work | `bay start <change>` (it is open again) |
| Read state, and main commits that did not merge through bay (`unattributed`; `by_writer` counts them per committer) | `bay status` (lists live changes only: your own described, at most 20; the rest are named in `detail.more_changes`, and `bay status <change>` reads any one, merged too) |
| Reverse a merged change with a new, checked change | `bay undo <change> [--wait[=<seconds>]]` |

After a merge, a plain `bay start <intent>` opens the next change. `bay start <change>` picks up an
existing change.
A view is its actor's: the actor that made it (`BAY_ACTOR`, else the user name). In another actor's
view, `bay merge`, `bay save`, `bay start <change>` and the hook's check of an edit or a `git commit`
are refused `VIEW_NOT_YOURS`, which names that actor. Work in a view of your own, or take that one over
first with `bay start <change> --adopt`.
`<intent>` says what the work is, in a few words (or a card id). `--paths` lists every path the change reads and writes:
the view holds only those (`--paths .` is the whole tree). In a workspace, also declare its root files and
lockfile (such as `package.json`, `pnpm-workspace.yaml`, `pnpm-lock.yaml`, `Cargo.toml`, `go.work`): without them
a tool run in the view can use the main checkout's, and a lockfile change goes there, not into your change. Before `bay merge`, commit your work in the view
with a message saying what you did, and an `Open: <item>` line at its end for anything left undone or
needing a person: main's commit keeps that message, and its first line is main's title, so write it last,
saying what the work became.

More commands (adopting, lanes, outside branches, saving, approving, keys, flow, targets, holds), and
notes on views, overlaps and questions: read `commands.md` in this skill when you need one.

## Wait without polling

- **Wait without polling.** To hear when a change merges, run `bay merge <change> --wait=3600` as a background task
  (it does not submit it again, and you are woken when it returns), or let `[plugin.wake]` wake the session.
  Never read `bay status` again and again: each read is a turn, and one that finds nothing new says so.
- **What needs you comes to you.** An open refusal of your change is in the session's first lines, and a
  talk or an outcome comes as a `bay:` line at your next step. A session that hears no `bay:` line has
  nothing waiting on it: do not read `bay status` to make sure.

## One script per change

When you run commands from a script (a code mode), do the whole change in one script and end it:

1. `bay --json start <intent> --paths <paths>`: read `change` and `detail.view` from stdout.
2. Edit the files under the view, then `git -C <view> add -- <paths>` and `git -C <view> commit -q -m <message>`.
3. `bay --json merge <change>`: it returns at once.
4. Start `bay --json merge <change> --wait=3600` as a background task, and end the script: its result wakes you.

Stop at the first exit code that is not 0, and do that result's `next_step`.

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
- A `merged` result with `detail.batch` merged on one check of several changes' combined tree, not of
  yours alone. Report it merged, and name the batch.
- A `blocked` or `failed` result names a `reason`, the actions still `allowed`, and `retry_when`,
  one of seven: `by_itself` (bay retries; do nothing), `after_wait` (another change, session or
  holder finishes first), `after_fix` (repair the change), `after_person` (a person approves or
  lifts a hold), `after_setup` (install, keys, config, an upgrade or the network), `after_new_main`
  (main must change first), `never`. `next_step` says the exact step. bay refuses an unchanged
  retry, except where it retries itself.
- A refusal judges the path, not where you stand. Another directory, an absolute path or a shell redirect
  gets the same answer: do `next_step`, or ask the user. Never route around it.
- Unknown is not success. A missing, unreadable or unexpected result is a failure to report.

`allowed` uses four words. What each one means:

| Action | Do |
| --- | --- |
| `read` | Read what `detail` names, such as the new version of an input. |
| `refresh` | `bay start <change>`: brings in newer main and reports what moved. |
| `repair` | Edit in the view, then `bay merge` again. |
| `wait` | Nothing yet. `bay merge <change> --wait=3600` as a background task; it wakes you. |

Every refusal carries `next_step`: do that. `bay help --json` lists every reason with its next step.

Every other field and reason (red main, conflicts, undoes, disk, stale reads, warnings, and the reasons
that need more than their next step: `CHECK_FAILED`, `POLICY_INVALID`, `APPROVAL_PENDING` and others): read
`results.md` in this skill when a result names one.

## More in this skill

- `together.md`: talking to other agents, handing work over, several repositories, and a person who is not a developer.
- `setup.md`: setting up a repository, keeping merges fast (policy and config), what bay guards, rolling out hooks, a team.

## Never

- Never push main, merge into main, or move main yourself. bay merges.
- Never push a target such as `staging` (`[[target]]` in the config). bay's hook refuses it.
- Never run `git reset --hard`, `git clean -f`, `git branch -D`, `git checkout .`, `git restore .`, a force push,
  a push of `--all` or `--mirror`, or a fetch of `refs/bay/*` into another namespace. bay's hook refuses them,
  also inside `bash -c`.
- Never merge, cherry-pick, check out or restore from a lane's refs (`[[lane]]` in the config) in a
  view that is not of that lane. bay's hook refuses those commands, and `bay merge` refuses lane commits.
- Never edit or commit in another actor's view. Take it over with `bay start <change> --adopt` first.
  bay's hook, `bay merge` and `bay save` refuse it.
- Never approve on your own, or ask for an approver's key. Only the person approves: run
  `bay approve` only when they ask, on their machine, where their key signs it.
- Never grant a key, widen the policy, quarantine a check, or set `allow_flaky` to get a change through.
  Run `bay enrol` only when the user sets up keys.
- Never run `bay install` to merge work. It sets up a machine or a clone.
- Never report a change as merged until the result says `merged`.

## This skill

`bay install` copies the copy of this skill that matches the installed bay. `bay doctor` reports
`SKILL_MISMATCH` when they differ; run `bay install` again. Source:
https://github.com/gudjon/bay/tree/main/skills/bay

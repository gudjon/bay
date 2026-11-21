# bay: working with others

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
| See every machine and session on the repository, who works on red paths (`on_red`), the open talks (`talks`, newest first inside 24 KiB, `talks_more` counts the rest; each session's `owes_talks`), and the runners with their age (`runners`; `stale`); each machine's last `bay doctor` verdict (`doctor`; `null` if it never ran) and the fleet's count (`ready`: green, red, never ran, lowest bay version) | `bay status --all` |
| Read the line, head first, or this machine's last doctor verdict, at once with no call to the remote (`detail.from_store`; how old: `freshness`, or the verdict's `age_s`; `detail.arrivals_listed` false: the changes waiting for a runner were not read; `detail.sweep` not null: the line's copy was past its window, and the read started the sweep that refreshes it) | `bay queue --deadline 0`, `bay doctor --deadline 0` (without it, both read the remote) |
| See every head: each branch on the remote, classed `main`, `target`, `lane`, `merged`, `stale` or `unique`, and in `detail.stranded` the local branches no remote holds and worktrees whose folder is gone; deletes nothing | `bay status --branches [--stale-days <n>]` |
| Delete only the remote branches `merged` by `ancestor` or `patch`, each archived first and restorable from `.git/bay/pruned-heads.log`; a `[plugin.prune]` plugin runs after each delete (`detail.pruned[].on_prune`) | `bay prune [--stale-days <n>]` |
| See merges, events (`main_red`, `main_red_cleared`, `outside_merge`, `main_not_linear`, `target_moved`, `target_unpromotable`, `talk_arrived`, `talk_answered`, `talk_closed`, `wake`) and invalidations since you last looked | `bay status --since <cursor>` (pass the returned cursor next time) |
| Ask another agent (or a person) one thing; the reply is owed by the policy's default time | `bay talk <who> "<what>"` |
| Ask for one change, artifact or check instead, or name the reply time | `bay talk <who> "<what>" --change <id>\|--artifact <path>\|--check <id> [--follow-up <time>]` |
| Ask several at once, as one round (`--to-live`: every actor with a live session but you) | `bay talk --to <a> --to <b> "<what>"` |
| See a round: one row per recipient, its standing and its answer | `bay talk --round <id>` |
| Answer a discovery talk addressed to you (one decision) | `bay talk --answer <id> <value>` |
| Reply to a delivery or reliability talk addressed to you: its status and what shows it | `bay talk --backbrief <id> ready\|blocked\|validated\|done [--evidence <ref>]` (`blocked` also takes `--reason <CODE> --follow-up <time>`) |
| Read a talk and every reply to it, in order: who wrote each, when, an answer's value, a backbrief's status | `bay talk --show <id>` |

Run `bay talk --help` for the fields of each kind. One talk asks for one thing. The same ask again while
its talk is open is that talk (`state: already_sent`); once it is closed, asking again starts a new one.
A name is lower-case letters, digits and `-`. Under `signing = "approvals"` any such name is sent, and a
name no record names comes back in `detail.unseen` with a `detail.warning`: check it for a typo. Under
`signing = "all"`, `who` and `--decides` are an enrolled agent or approver.

- The notice of a talk to you names its one reply command: `--answer` for a discovery talk,
  `--backbrief` for a delivery or reliability talk. Send a new backbrief when the status changes;
  the same one again writes nothing. The sender is told the status word, never your text.
- A reliability talk closes once your latest backbrief is `done` or `validated` with `--evidence`
  naming a commit on main by its hash. A delivery talk closes when a merge names its change in a
  `Fixes:`, `Reverts:` or `Supersedes:` trailer, or the same way as a reliability talk when the
  commit your evidence names is a bay merge.
- A talk read after its `--expires` is void: the notice prints `EXPIRED`, and it owes no reply.
- A talk event in `bay status --since` carries the reply: `answer` (the answer's value) and `reply` (the
  backbrief's `status` and `evidence`). A closed talk says `standing: "closed"` and owes nothing. With no
  cursor, a talk that closed in the last week is listed once.
- `bay merge` and `bay undo` list the open talks about that change in `detail.open_talks`. Read them;
  they never block the merge.
- With a `[plugin.wake] command`, bay wakes your session once for a talk to you, and once when a runner
  merges your change, blocks it on a step that is yours, or finds it conflicts while it waits in line
  (`cause: "outcome"`). Your repository's command may start a session when yours has ended. The plugin's answer
  is the event's `wake` in `bay status --since`. An outcome your own `bay merge --wait` returns wakes nothing.

## Hand work over to another agent

A handover is a message, not a merge. Never make the next agent wait for your last change to merge.

- **Start the next agent now.** Put the task card's text, or its path and change id, in the prompt that starts
  it. Submit the card's change too; the two run side by side.
- **Its first change waits, not the agent.** It opens each change that builds on yours with
  `bay start <intent> --paths <paths> --after <change>` and submits at once. bay merges it after yours, and
  `bay merge` returns `DEPENDENCY_PENDING` without holding the agent.
- **Unfinished work moves with its change.** `bay abandon <change> --why "handed to <agent>"`, and the next
  agent runs `bay start <change> --adopt`.

## Work in more than one repository

Each repository is its own bay: its own main, queue, checks, changes and talks. Nothing in one moves the other.

- **Run each command in that repository's own clone.** A change id names a change of one repository only.
- **One change, one repository.** Work that touches two repositories is two changes, one in each. Name the other
  change's id in each intent, so a reader finds the pair.
- **`--after` follows only a change of the same repository.** A change of another repository never merges here,
  so a change that names it waits for ever. When work needs another repository's change first, run
  `bay merge <change> --wait` in that repository's clone, then submit the dependent change in its own.
- **Keep both moving.** Submit each change as soon as it is ready; a repository's queue never waits for the other's.
  Read `bay status` in each repository: a red main in one says nothing about the other.

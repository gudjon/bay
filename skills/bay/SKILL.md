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
| Pick a change up again: bring in newer main and see what moved | `bay start <change>`. When it remade an unfinished view, `detail.unsaved_kept` names where its unsaved files were kept: copy them back, then `bay merge` |
| Take another actor's change and its view over, when that actor gave it up or is gone | `bay start <change> --adopt` (you become its actor; `detail.adopted_from` and `detail.view_adopted_from` name the one before; while that actor still works, after the takeover window) |
| Fix a change that already merged | `bay start <intent> --fixes <change>` (the merge names it in a `Fixes:` trailer) |
| Try work that must never merge, in a sandbox lane the config declares | `bay start <intent> --paths <paths> --lane <lane>` (`bay merge` runs its checks; it never merges) |
| Take in a branch made outside bay (a bot's or a person's) and merge it in one step | `bay merge --from <branch>` (its intent is the branch's name). To take it in first: `bay start <intent> --from <branch>` (the remote's tip when the remote has the branch; it declares the files the branch changes, or give `--paths` covering them; then `bay merge`). After a rebase: `bay start <change> --from <branch or sha>` replaces its draft, no new branch name. Main's commit keeps the branch's author and messages |
| Merge a change only after another change merged | `bay start <intent> --paths <paths> --after <change>` (repeatable; submit it now, it merges once each one merged) |
| Save edits made in the main checkout (non-developers) | `bay save <intent>` |
| See the changes waiting for this machine's approver: candidate, paths, diff summary | `bay waiting` (`detail.waiting`) |
| Approve one, or say no, when the approver asks you to | `bay approve <change> [--why <text>]`, `bay reject <change> [--why <text>]` |
| Waive one failed check marked `waivable`, when the approver asks you to | `bay approve <change> --waive <check> --why <text>` (that candidate only; never a pass) |
| Add one key to the key registry, when the user sets up keys | `bay enrol <class> <name> <key.pub>` (class `registry`, `approver`, `reviewer`, `producer`, `runner` or `agent`; signed by `bay.registryKey`) |
| Remove a key, when the user asks: `rotated` keeps its past records valid, `revoked` (compromised) voids them all | `bay enrol --remove <class> <name> --reason rotated\|revoked` (signed by `bay.registryKey`; the last registry key is never removed) |
| Give the change up, so another agent may adopt it | `bay abandon [<change>] [--why "<reason>"]` (default: the view's change; say why, e.g. where the work merged) |
| Start over after abandoning, with a fresh change | `bay start <intent> --new` |
| Read state, and main commits that did not merge through bay (`unattributed`; `by_writer` counts them per committer) | `bay status` (lists live changes only: your own described, at most 20; the rest are named in `detail.more_changes`, and `bay status <change>` reads any one, merged too) |
| See the flow: live changes by actor and their limit, merges this week, cycle time, queue wait and check time (p50, p90), the oldest live change, what waits for a yes | `bay status` (`detail.flow`) |
| See each promotion target (`staging`): its tip, `drift`, `lag`, checks, freeze and the `promotable` commit | `bay status` (`detail.targets`) |
| See whether main is held, by whom (`source`: `window` or `plugin`), why and until when | `bay status` (`detail.hold`) |
| Reverse a merged change with a new, checked change | `bay undo <change> [--wait[=<seconds>]]` |

After a merge, a plain `bay start <intent>` opens the next change. `bay start <change>` picks up an
existing change.
A view is its actor's: the actor that made it (`BAY_ACTOR`, else the user name). In another actor's
view, `bay merge`, `bay save`, `bay start <change>` and the hook's check of an edit or a `git commit`
are refused `VIEW_NOT_YOURS`, which names that actor. Work in a view of your own, or take that one over
first with `bay start <change> --adopt`.
`<intent>` says what the work is, in a few words (or a card id). `--paths` lists every path the change reads and writes:
the view holds only those (`--paths .` is the whole tree). Before `bay merge`, commit your work in the view
with a message saying what you did, and an `Open: <item>` line at its end for anything left undone or
needing a person: main's commit keeps that message. Research, a plan or a review a person should see is a
short file (`notes/<date>-<what>.md`), saved like any other change. When `bay merge` names
`detail.main_changed`, main changed those files of yours since your change began: read its `commit`.
When you need a person's decision, ask with your product's question tool (Claude Code's
`AskUserQuestion`), not in words: bay records that you wait for one, and why. With
`[agent] question_wait_s` in the policy, a person may answer it from elsewhere; it reaches you as theirs.
A list every change appends to (cards, a registry) merges both sides' lines when main's
`.gitattributes` marks it `merge=union`; your change's own `.gitattributes` never decides its merge.
`bay start` lists in `detail.overlaps` the live changes whose
declared paths overlap yours (change, intent, actor, paths). It does not refuse; talk to them, or
narrow your paths. `bay help` shows every command and flag on one page, with what each one writes and
never writes (read it before you hesitate to run one), and `bay <command> --help`
says more about one. `bay help --json` is the same list for programs, with every reason code and
the JSON Schema of `bay.result/1`.
It also lists each agent product's hook moments and what each can block, and in `talk` how a talk is
stored (`bay.talk/1`: its ref, its fields and each reply's fields), for a program that reads talks.

## Keep merges fast

bay exists to make merging faster than doing it by hand: you submit once, and the runners merge for you. How they do it:

- **One line, checked in batches.** Runners take the change at the head of the line, together with the changes behind
  it that merge cleanly with it, and check them as one batch. A batch's size follows how often recent checks failed.
- **Checked ahead.** While one batch is checked, other runners check the next batches on the main the batch ahead
  will make. When a batch's turn comes and main is as predicted, it merges at once on that check.
- **Placed at once.** A runner places a new change in line within seconds, even while it checks another.
- **Skips, not stops.** A change that conflicts, may not join a batch, already failed, or waits for a red main is
  passed over and keeps its place for its own turn.

What you do decides the speed for every agent:

- **Submit once, then go on with other work.** `bay merge` once. Running it again does not move the change up.
  Read the outcome with `bay status <change>` or `bay status --since`, not in a tight loop.
- **Fix in place.** When a change is refused, fails a check, or `bay status` shows `operation.conflict`:
  `bay start <change>` (it brings in newer main), fix it in the view, then `bay merge <change>`. The new revision
  keeps the change's place in line, even at the head. Never abandon it and start a new change for the same work:
  a new change joins at the back.
- **Green before you submit.** Run the repository's fast checks in your view first. A red change costs the whole
  batch it is in a split to find it.
- **Small and narrow.** One intent per change, only the paths that intent needs.
- **Never push main, and never merge on the host.** Every commit that reaches main outside bay throws away the batch
  being checked and the batches checked ahead of it.
- **Changes to `.bay/` go through bay** like any other change, in a change of their own; never by a pull request or
  a direct push.
- **A fix for a red main goes first.** `bay start <intent> --fixes <change>` (or `bay undo <change>`) runs ahead of
  the line.
- **Give up dead work.** `bay abandon <change> --why <reason>`: a runner frees its place at its next pass.
- **Name `--input` only when the work relies on that version**; such a change is checked on its own.

Signals to read: `detail.batch.stopped` says why a batch ended; `detail.warnings` names `wake_failing` when no
session is being told of its refusals; `waiting_for` says what a queued change waits for.

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
- `bay status` lists in `detail.red` each check that turned main red after a merge, with its `culprit`
  once the evidence names one merge. If that is yours, fix it first: `bay start <intent> --fixes <change>`.
- While a change waits in the queue, `bay status <change>` may show `operation.conflict`: main
  moved into a `MERGE_CONFLICT` on the listed paths. It keeps its place; refresh and repair now.
  The agent hook also tells your session once.
- A `merged_on_your_paths` notice: another change merged on paths you declared. Refresh, re-read
  the named paths, then go on.
- `MAIN_RED` with `waiting_for: main`: main is broken on your paths. Your change keeps waiting in its place
  in line and is checked again once main is green, before the changes that came after it; do not merge it
  again. A change that fixes it (`bay start <intent> --fixes <owner>`) goes ahead of the line.
- A change that no check covers, such as a document no check reads, merges in the next push.
- `bay status` describes your own changes and counts the others; `bay status <change>` reads one.
- `bay status` with `detail.warnings` naming `no_live_runner`: work is queued and no runner has
  been seen recently. Tell the user a runner must run (`bay runner run`, or
  `bay runner install` to keep one running). The work stays queued. The agent hook says the
  same once, at session start or a prompt: "bay: no runner is running for this repository".
- `bay status` with `detail.warnings` naming `main_not_linear`: a merge pushed outside bay put
  merges off main's first-parent line. bay still finds them. Tell the user to require linear
  history on the host.
- A `blocked` or `failed` result names a `reason`, the actions still `allowed`, and `retry_when`,
  one of seven: `by_itself` (bay retries; do nothing), `after_wait` (another change, session or
  holder finishes first), `after_fix` (repair the change), `after_person` (a person approves or
  lifts a hold), `after_setup` (install, keys, config, an upgrade or the network), `after_new_main`
  (main must change first), `never`. `next_step` says the exact step. bay refuses an unchanged
  retry, except where it retries itself.
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
| `CHECK_FAILED` | A failed check ends the run: each check after it is `not_run` in `detail.checks`, never report it passed. A check with `baseline = "base"` names only new failures; pre-existing ones appear as `baseline_failures` in `coverage_gaps`: report them, never as passed. A check with `waivable = true` may instead get an approver's waiver (`bay waiting` lists it); then `bay merge --wait`. The waiver appears in `coverage_gaps` with the real verdict: report it, never as passed. |
| `CHECK_FLAKY` | A merge may name one flaky in `coverage_gaps`: report it, never as passed. |
| `WIP_LIMIT` | From `bay start`: you hold the most live changes (`detail.live_changes`). From `bay merge`: the review queue is full (`detail.review`: `waiting`, `max`, `oldest`); do `detail.next`. |
| `POLICY_INVALID` | While main's newest policy breaks a rule, bay judges each change by the last valid policy on main and names it in `detail.policy_used`: report that. A fix to the policy then merges like any change, and must still raise main's `revision`. Your own change's broken policy, or a `.bay/config.toml` that does not load, is refused: repair it. While main's config does not load, bay uses the last valid one and names it in `detail.config_used`. Only when main never had a valid policy is every merge blocked, its repair too. That repair merges when signed by the registry key: tell the user; on the machine that holds it, `git config bay.agentKey <registry key>` for that one `bay merge`, then set it back. Never do this unasked. On a main from `bay host` this path is not yet proven: tell the user so. Under `signing = "approvals"`, a fix to only the policy and its check scripts runs its own scripts and waits for an approver's yes instead. |
| `VIEW_NOT_YOURS` | `detail.view_actor` made the view, `detail.actor` is you (`detail.actor_source`: from `BAY_ACTOR`, or the user name). Take the change over only when that actor gave it up or is gone: ask it first (`bay talk`). Sessions with no `BAY_ACTOR` on one account are one actor. |
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
| See every machine and session on the repository, who works on red paths (`on_red`), the open talks (`talks`, newest first inside 24 KiB, `talks_more` counts the rest; each session's `owes_talks`), and the runners with their age (`runners`; `stale`); each machine's last `bay doctor` verdict (`doctor`; `null` if it never ran) and the fleet's count (`ready`: green, red, never ran, lowest bay version) | `bay status --all` |
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
- A talk read after its `--expires` is void: the notice prints `EXPIRED`, and it owes no reply.
- A talk event in `bay status --since` carries the reply: `answer` (the answer's value) and `reply` (the
  backbrief's `status` and `evidence`). A closed talk says `standing: "closed"` and owes nothing. With no
  cursor, a talk that closed in the last week is listed once.
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
   migration), add a `[[hold]]` with `end` and `reason` (and `start`, to book a window ahead) (a `target` holds a promotion target instead), or a
   `[plugin.hold] command` that answers `{"held": true|false, "reason": "..."}` to `<command> status`.
   To act on the host when `bay prune` deletes a branch (close its pull request,
   say), add `[plugin.prune] command`: it runs as `<command> pruned <branch> <sha> <archive ref>`.
   A `dev → staging → production` repository serves `dev` (the remote's default branch), with
   `[[target]] name = "staging"` (from main) and `[[target]] name = "production"` with `from = "staging"`:
   changes merge into `dev`, and `bay status` names each target's promotable commit.
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
   signed, and no machine needs a `runner`, `producer` or `agent` key. Each clone takes its own
   producer name (`local-<id>` in its git config); set `bay.producerName` only to choose one.
   There, and with no registry, evidence reuse and batching stay off unless the policy also says
   `[policy] reuse_unsigned = true`. Results still name their trust: `unsigned` or `approvals`.
   To keep changes small, add `[limits]` with `max_file_mib`, `max_files` or `max_added_lines`
   (positive integers; each optional). To cap the review queue, add `max_waiting_approvals`:
   with that many changes waiting for a yes, a change that needs one more is refused `WIP_LIMIT`.
   A check that takes seconds can say `first = true`: it runs before the others, and a failure there
   ends the run before any slow check starts.
   To merge more changes a day (https://getbay.io/docs/throughput), in `[runner]` of `.bay/config.toml`:
   `ahead = N` lets runners that do not hold the head check the next N changes early, on the main the
   changes ahead of them predict; `batch = { min = 2, max = 8, max_failure_rate = 0.3 }` checks several
   queued changes as one tree, merges them in one push, and splits a red batch to find the culprit;
   `footprint_lanes = true`, with `footprint = true` on a check whose verdict reads only its `paths`
   (else its `when_paths`), carries its records across unrelated merges and checks a change beside the
   head. These three turn on evidence reuse, and need a registry that signs everything or
   `[policy] reuse_unsigned = true`. `parallel_checks = N` (1 to 16) runs one change's checks at
   once, with no reuse. A required check with no `when_paths` makes every change's footprint the
   whole tree, so no change is checked beside another. Each runner is its own check slot: more runners give more early checks. When
   `bay status` `detail.flow` shows queue wait well above check time, add a runner or one of these.
   A check reads `turn` in its request (`head` or `ahead`), so a busy worker can serve the head first.
   A slow check that reads the whole tree can run after merge instead of on every change:
   `stage = "post"` in its `[[check]]`, and `[runner] post = true` in `.bay/config.toml`. The runner then runs it
   once on main's newest tip, and with `bisect_post = true` a red result names the merge that broke it.
   With `[policy] evidence_reuse = "exact-inputs"`, `footprint = "observed"` on a `[[check]]` lets bay record what each run
   read and listed; a later candidate whose bytes there are the same reuses the result, with no read set declared.
   With `[runner] warm = true`, the runner also runs such checks once on main's newest tip, so a change runs only the
   checks whose reads it touches.
   For files that never merge (images, models, binaries), mark them `lockable` in `.gitattributes`
   on main (`art/** lockable`, Git LFS's attribute): one session at a time edits each. Your first edit of one takes its lock on the
   remote; elsewhere the edit is refused `PATH_HELD`, naming the holder (`bay status --all` lists every
   lock held now in `detail.locks`). Edit other paths, or
   wait until that session's turn ends. Offline, the edit is refused `REMOTE_UNREACHABLE`. A
   lock needs the newest copy: when main changed the file after your work began, the edit is
   refused `INPUT_CHANGED`; bring in main (`bay start <change>`), then edit. A change to such a
   file merges only while it holds the lock (after its turn ends too, until another session
   takes it); otherwise it is refused `LOCK_NOT_HELD`: edit the file again to take the lock.
   `bay save` takes the locks of the files it saves.
   To keep large files out of Git, add `[plugin.assets]` with `over_limit = "offload"` to
   `.bay/config.toml`: a new file over `max_file_mib` goes to the remote's own store (a `bay host`, a
   hosted product, or a path remote, through the remote's own login), and `.bay-assets` names its bytes
   (`sha256:<hex> <size> <path>`). A plain Git host keeps no store: there, name one with
   `command = "<your store's script>"`. `*.pdf bay-offload` in `.gitattributes` on main
   sends files of a kind to the store whatever their size. A change's own `.gitattributes` never
   locks, unlocks or offloads its own files: main's says. The file stays in
   the folder; never add it by hand. Other clones get it at `bay install`, views and checks
   when they are made. bay ships a store for any folder or shared drive:
   `examples/assets/folder-store`, named as `command = "scripts/folder-store <folder>"`. A merge
   whose large files the store does not hold is refused `ASSET_MISSING`: save them again. A file the
   store purged keeps its line in `.bay-assets`; a clone names it and goes on. To let a store drop
   generated files after a time, add `[assets] keep = [{ paths = "renders/", days = 30 }]` to the policy.
   To stop any purge of files that must be kept (a legal hold), add `hold = ["matters/acme/"]`: a file
   ever saved under a held path stays, even if deleted since. A store asks `bay assets held <id>` before it purges.
   A hold needs an approval `[[check]]` with `when_paths = [".bay/"]`, so only a person's signed yes lifts it.
3. Before you commit a `[[check]]` command or a plugin (`[plugin.wake]`, `[plugin.notify]`,
   `[plugin.assets]`), prove it: `bay plugin test check|wake|notify|assets -- <command>`. It runs bay's contract cases the
   way bay runs it, changes nothing, and exits 0 only when every case passes.
   `detail.cases` names each failing case and why.
4. With full signing, each agent's machine sets `bay.agentKey`, and the agent runs with `BAY_ACTOR=<its enrolled
   name>`: the host refuses a change signed by one agent for another actor (the default actor is Git's user name).
5. In one clone: `bay install`, then merge the agent settings file it writes; other clones need no
   step. It puts this skill in every Claude
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
  `bay install`. The hook then refuses a push to main, a force push, `reset --hard` and the like, and an edit
  or a `git commit` in another actor's view. Without it,
  `bay doctor`'s `hooks` row reads `off` and an agent can push main around bay.
- **Everything that reaches main, only on a bay-hosted main** (`bay host`). On a forge such as GitHub, a merge
  made in its web page or API, and a script's push, never pass a local hook; protect main there so that only the
  runner may push it.
- bay sees what went around it: `bay status` counts it in `detail.unattributed` (per committer in `by_writer`), and `bay doctor` is red when main
  bay merges into also holds such merges. Tell the user; do not work around it.

## Roll bay's hooks out to many machines

The order, learned in a fleet rollout:
1. On every machine: `bay update`, then `bay install --machine`. It writes only the bay program and its guide, in
   every Claude config directory it finds (`bay.claudeConfigDirs` names others). It never writes hook entries, a
   repository's settings or its Git config, so it is safe to run on a busy machine. A pin (`bay.updateVersion`) is the
   oldest version a machine runs: it follows every newer release, and an install by a bay below the pin or never
   released is refused `UPDATE_PINNED`, so tests and other agents never move a shared runner. An image that must
   keep one bay (a server or runner container) turns updates off with `BAY_DISABLE_AUTOUPDATE=1`: a pin is
   only a floor, and `bay status` and `bay doctor` update hourly. A runner moves its own renamed config keys
   when it starts.
2. Put `[agent] product = "claude"` in `.bay/config.toml` on main. Each machine's `bay doctor` now reads hooks red
   until step 3 merges: that is expected, not a fault. After a bay update, doctor's hooks row names under `behind` any
   hook this bay adds that the project's settings lack: repeat step 3.
3. In one clone, run `bay install`, and merge the change that commits `.claude/settings.json` with bay's entries.
   Every machine gets the hooks as it takes that change. A machine without bay is unaffected: each entry first
   checks that bay is present.
4. Run `bay doctor` on each machine after it takes the change. `bay status --all` then shows every machine's
   verdict in one place (`ready`); fix each red machine it names, and do not guess.

## Set up bay for a team (agent-led)

When the user asks to set bay up for a small team, do each step for them. Tell them `say` after each.

1. On each person's machine: install bay (see "Before you start"), then `bay install` in their copy.
2. Put `.bay/config.toml` and `.bay/policy.toml` on main and push them. The policy says
   `[policy] signing = "approvals"`, with an approval `[[check]]` for the paths that need a yes.
3. The person who keeps the registry runs, on their own machine in the project's clone:
   `bay install --approver <name>` (a short lower-case word). It makes their keys if they have
   none, starts the registry, enrols them, and reads it back from the remote. Re-running it is safe.
4. For each other person who approves: they send only their public key, the `.pub` file, and the
   registry keeper runs `bay enrol approver <name> <their key.pub>`.
5. On each other approver's machine: `git config bay.approverName <name>` and
   `git config bay.approverKey <their private key path>`.
6. `bay doctor` on each machine. It names anything missing, with its fix, and publishes its verdict, so
   `bay status` then `bay status --all` on any one machine shows every machine's (`ready`).

`bay enrol` refuses a key that is already enrolled, and says why. Nothing is written then.

## Never

- Never push main, merge into main, or move main yourself. bay merges.
- Never push a target such as `staging` (`[[target]]` in the config). bay's hook refuses it.
- Never run `git reset --hard`, `git clean -f`, `git branch -D`, `git checkout .`, `git restore .`, a force push,
  or a push of `--all` or `--mirror`. bay's hook refuses them, also inside `bash -c`.
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
https://github.com/gudjon/bay/blob/main/skills/bay/SKILL.md

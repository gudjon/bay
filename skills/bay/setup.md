# bay: setting up and keeping merges fast

## Keep merges fast

bay exists to make merging faster than doing it by hand: you submit once, and the runners merge for you. How they do it:

- **One line, checked in batches.** Runners take the change at the head of the line, together with the changes behind
  it that merge cleanly with it, and check them as one batch. A batch's size follows how often recent checks failed.
- **Checked ahead.** While one batch is checked, other runners check the next batches on the main the batch ahead
  will make. When a batch's turn comes and main is as predicted, it merges at once on that check.
- **Placed at once.** A runner places a new change in line within seconds, even while it checks another.
- **Unrelated changes pass.** A change whose files lie outside what the checks of every change ahead of it read
  is checked and merges while those checks still run; the changes it passed keep their results.
- **Skips, not stops.** A change that conflicts, may not join a batch, or already failed is passed over and keeps
  its place for its own turn.
- **A red main holds nothing.** A change on a red check's paths runs that check before it merges, judged against
  main: it merges unless it adds a failure main does not have.

What you do decides the speed for every agent:

- **Submit once, then go on with other work.** `bay merge` once. Running it again does not move the change up.
  Read the outcome with `bay status <change>` or `bay status --since`, not in a tight loop.
- **Wait without a loop.** Plain `bay merge` returns at once, and with `[plugin.wake]` the outcome wakes your
  session. When one command may only run for a while, use `bay merge --wait=<seconds>` below that limit, then
  `bay status <change>`; the run goes on meanwhile.
- **Fix in place.** When a change is refused, fails a check, or `bay status` shows `operation.conflict`:
  `bay start <change>` (it brings in newer main), fix it in the view, then `bay merge <change>`. The new revision
  keeps the change's place in line, even at the head. Never abandon it and start a new change for the same work:
  a new change joins at the back. A revision sent while the change's batch is being checked waits: the checked
  one merges with the batch, and your next `bay merge` sends the newer edits as the next change
  (`detail.follow_up`).
- **Green before you submit.** Run the repository's fast checks in your view first. A red change costs the whole
  batch it is in a split to find it.
- **Small and narrow.** One intent per change, only the paths that intent needs.
- **One live change per file.** When `detail.overlaps` at `bay start` names another live change of yours on the
  same file, finish that one first or put the work in it: two changes adding rows to one table conflict.
- **Never push main, and never merge on the host.** Every commit that reaches main outside bay throws away the batch
  being checked and the batches checked ahead of it.
- **Changes to `.bay/` go through bay** like any other change, in a change of their own; never by a pull request or
  a direct push. Keep a check's `adapter` files apart too: a change to the policy or to them runs its checks twice,
  as main has them and as it has them.
- **A fix for a red main goes first.** `bay start <intent> --fixes <check>` (the red check), `--fixes <change>`
  (or `bay undo <change>`) runs ahead of the line. A change already open that turns out to be the fix is named
  so in place, keeping its id: `bay start <change> --fixes <check|change>`.
- **Give up dead work.** `bay abandon <change> --why <reason>`: a runner frees its place at its next pass.
  Work whose actor is gone: `bay start <change> --adopt` (at once when it was idle past the takeover window),
  then merge or abandon it.
- **Name `--input` only when the work relies on that version**; such a change is checked on its own.
- **Move a tree in three changes.** A check runs as main has it, so first merge a change that teaches the checks
  both the old and the new path; then the pure move, no rewrites; then drop the old path. Before each submit, take
  main's copy of every file `bay merge` names as the last step, and run main's own check on the result.
- **Edits outside a sparse view count.** `bay merge` stages them; your own `git add` there needs `--sparse`.

Signals to read: `detail.batch.stopped` says why a batch ended; `detail.warnings` names `wake_failing` when no
session is being told of its refusals; `waiting_for` says what a queued change waits for. A `CHECK_UNKNOWN`
names why the check gave no answer in `say`, and `bay status` shows it as the receipt's `why`. When a check gives
your own `bay merge --wait` no answer on this machine and a live runner serves it, that runner checks it instead:
the wait goes on, and `detail.checked_elsewhere` names the hand-over. A check that failed is never handed over.

A repository's maintainer sets it up for speed in `.bay/` (details under "Set up a repository"):

- **Give every check `when_paths`.** A change no check covers is merged by `bay merge` itself, in seconds. One
  check without `when_paths` sends every change, notes included, through the runners' line.
- **Hold sizes with `[limits]`, not with a check.** bay judges them at submit, with no runner.
- **Split a slow check by part**, each with its own `when_paths`, so a change runs only the checks its files reach.
- **Say what each check reads**, so unrelated changes pass it: `footprint = "observed"` (bay records the reads),
  or `materialize = "changed"` for a check that judges only the change's own files. A check with neither is read
  as reading everything, and nothing passes a change that runs it.
- **Add `[plugin.wake]`**, so sessions hear outcomes instead of asking for them.
- **Serve each check `environment` from two runners or more**, on machines with CPU to spare, and set `ahead`,
  `batch` and `parallel_checks`. `evidence_reuse = "exact-inputs"` with `footprint = "observed"` on a check
  reuses a result whose reads did not change; `[runner] warm = true` has one ready on main's newest tip.
- **Keep each store healthy.** Run the command `bay runner install` prints, which schedules hourly Git
  maintenance, and act on what `bay doctor` names.

An example for a repository with a service, a web app and shared code. A change to `docs/` meets no check and
merges in seconds; a change to `apps/web/` runs `lint` and `web`, never `api`:

```toml
# .bay/policy.toml
revision = 1

[limits]
max_file_mib = 1

[[check]]
id = "lint"
environment = "linux"
command = "scripts/check lint"
adapter = ["scripts/check"]
when_paths = ["services/**", "apps/**", "libs/**", "scripts/check"]
footprint = "observed"
expected_minimum = 1
timeout_s = 300

[[check]]
id = "api"
environment = "linux"
command = "scripts/check api"
adapter = ["scripts/check"]
when_paths = ["services/api/**", "libs/**", "scripts/check"]
footprint = "observed"
expected_minimum = 1
timeout_s = 900

[[check]]
id = "web"
environment = "linux"
command = "scripts/check web"
adapter = ["scripts/check"]
when_paths = ["apps/web/**", "libs/**", "package.json", "pnpm-lock.yaml", "scripts/check"]
footprint = "observed"
expected_minimum = 1
timeout_s = 900
```

```toml
# .bay/config.toml
[policy]
evidence_reuse = "exact-inputs"   # reuse a result whose reads did not change

[runner]
ahead = 8                         # runners not on the head check the next changes early
batch = { max = 8, max_failure_rate = 0.3 }
parallel_checks = 3               # a change's checks run at once
warm = true                       # results ready on main's newest tip

[plugin.wake]
command = "scripts/wake"          # sessions hear outcomes; no polling
```

Then start a runner for `linux` on two machines or more (`bay runner install` on each). To see what a change
would meet before opening it: `bay start <intent> --paths <paths> --dry-run`.

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
   repository that wants no checks says `unchecked = true`. A check with `when_paths` runs only
   for a change to a path its globs match: `when_paths = ["**/AGENTS.md", "!notes/**"]` is every
   `AGENTS.md` outside `notes/`. A glob that starts with `!` drops the paths it matches, read in
   order as `.gitignore` reads it: the last glob that matches decides, so a later glob can take a
   path back, and the list starts with a glob that takes paths. Paths that need a person's approval
   get a `[[check]]` with `kind = "approval"` and `when_paths`, and no command; each approver is
   enrolled with `bay enrol approver <name> <key.pub>` and approves with `bay approve <change>`.
   It records a signed approval record of the exact candidate.
   A run check may say `waivable = true` (never an approval check): an enrolled approver may then
   waive it on one candidate with `bay approve <change> --waive <check> --why <text>`. A waiver
   is never a pass; the merge names it in `coverage_gaps`. Without a registry no waiver counts.
   A small team with no runner host sets `[policy] signing = "approvals"`: only approvals are
   signed, and no machine needs a `runner`, `producer` or `agent` key. Each clone takes its own
   producer name (`local-<id>` in its git config); set `bay.producerName` only to choose one. Two
   runners never share one: a runner started under a live runner's is refused `PRODUCER_SHARED`.
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
   `bay status` `detail.flow` shows queue wait well above check time over the week, add a runner or one of these;
   `operation.timing` of the slowest changes names the checks that took longest.
   A check reads `turn` in its request: `head` or `ahead` for a queued change, so a busy worker can serve the
   head first; `post`, `bisect` or `warm` for a run after merge, so a bisect step can keep the whole suite.
   A slow check that reads the whole tree can run after merge instead of on every change:
   `stage = "post"` in its `[[check]]`, and `[runner] post = true` in `.bay/config.toml`. The runner then runs it
   once on main's newest tip, and with `bisect_post = true` a red result names the merge that broke it.
   The post checks of different environments run side by side, one at a time in each; a runner moves
   to a new bay binary only once they end. While it is red, its request names `rerun`: the units that
   failed last. The check's command runs them too, and lists in `plan.gone` each one it no longer has;
   a run that answers neither is `unknown`, not green.
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
   command that starts it and schedules the repository's hourly Git maintenance; it starts
   nothing. Tell the user that command. `--print` only shows it.
7. A check that must read a directory outside its tree (a package store) needs it named on the
   machine that runs checks: `git config --global --add bay.checkReadPath ~/<dir>`. It is read-only.

## What bay guards, and when

- **Its own merges, always:** what merges through bay is exactly what was checked.
- **An agent's Git commands, only with the hook:** `[agent] product = "claude"` in `.bay/config.toml`, then
  `bay install`. The hook then refuses a push to main, a force push, `reset --hard` and the like, and an edit
  or a `git commit` in another actor's view. It refuses only on purpose: a hook that fails or does not finish
  lets the call go on, and merge and main's host hook judge every change. Without it,
  `bay doctor`'s `hooks` row reads `off` and an agent can push main around bay.
- **Everything that reaches main, only on a bay-hosted main** (`bay host`). On a forge such as GitHub, a merge
  made in its web page or API, and a script's push, never pass a local hook; protect main there so that only the
  runner may push it.
- bay sees what went around it: `bay status` counts it in `detail.unattributed` (per committer in `by_writer`), and `bay doctor` is red when main
  bay merges into also holds such merges. Tell the user; do not work around it.

## Roll bay's hooks out to many machines

The order, learned in a fleet rollout:
1. On every machine: `bay update` (its `detail.changes` lists what each newer release brings; it runs
   `bay install --machine` itself). That writes only the bay program and its guide, in
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
   checks that bay is present. A settings file that already carries bay's entries is left unchanged, so a clone
   whose `bay install` changes nothing has nothing to merge. The project may add its own conditions to an entry
   that never stops the agent (the wait for talk), between the presence check and `exec bay hook <event>`:
   doctor and `bay install` keep it as written. A guard entry changed that way reads as missing.
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

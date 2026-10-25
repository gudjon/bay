# Changelog

## 0.5.0

- Locks come from `.gitattributes` on main: mark a path `lockable`, as Git LFS does. `[locks]` is gone.
- A kind of file leaves Git by `bay-offload` in `.gitattributes`. `[plugin.assets] offload` is gone.
- An edit of `.gitattributes` made outside bay is named, as a policy edit is.

## 0.4.5

- Large files go to the remote's own store: `[plugin.assets]` needs no command.
- An https remote keeps them in parts, through Git's own login for that remote.
- `bay host` keeps them beside the repository; an ssh remote reaches them over Git's ssh.

## 0.4.4

- A push the remote refuses is named with the remote's reason, never as a moved change.
- A policy with `[assets] hold` needs an approval check over `.bay/`: only a person lifts a hold.
- One `bay approve` answers every approval check a change waits on; two no longer collide.

## 0.4.3

- With a key registry, a lock is signed by the agent's key; an unsigned lock holds nothing.
- A failed check ends the run: later checks are named `not_run`, so a cheap lint spares slow tests.
- `[assets] hold` keeps a file's stored bytes from any purge; a store asks `bay assets held <id>`.
- On a pinned machine, `bay install` from another bay version is refused `UPDATE_PINNED` and changes nothing.
- A runner moves its own renamed config keys at start, so it no longer runs as `local`.
- Doctor names each hook a newer bay adds that the project's settings lack, under `behind`.
- The question hook gets a 120 s timeout, so a hung bay never holds an agent for ten minutes.

## 0.4.2

- A push or fetch to a remote with many refs gets three minutes, not the local call's thirty seconds.
- `bay status --all` names why a session waits: a question, a permission prompt or its idle prompt.
- With a key registry, each question an agent asks becomes a talk; it closes when the session goes on.
- A person with an approver key answers an agent's question: `bay talk --answer <id> --value <answer>`.
- With `[agent] question_wait_s` set, a signed answer reaches the question tool as the person's answer.
- Update the host and runner to 0.4.2 before setting `question_wait_s`: an older bay refuses the key.

## 0.4.1

- A plain `git clone` saves and merges; no clone needs `bay install` of its own.
- main is the remote's default branch; a clone follows when that default moves.
- The skill says to save research, plans and reviews a person should read as a short file.
- A runner with a version pin never moves to another version when its binary changes.
- `bay merge` names the files main changed since your change began, and main's commit.
- A file main's `.gitattributes` marks `merge=union` merges both sides' lines; a change cannot mark its own.
- Under any agent product, an agent's own `bay save` acts as its session, so its own lock never holds it.
- A change's refusal is told to the session that holds it while it is live, not every agent.
- `bay status --all` names each session's agent product, and lists the locks held now with their holders.
- A change that would put back what another merged change replaced is refused `UNDOES_MERGED`, naming it.
- `[plugin.assets] offload = ["**/*.pdf"]` keeps a kind of file out of Git, whatever its size.

## 0.4.0

The last renames before the first user; scripts and configs that use the old names need the new ones.

- The key class `executor` is `runner`: `bay enrol runner`, `bay doctor --role runner`.
- Git config `bay.executorKey`, `bay.executorName` and `bay.executorSandbox` are `bay.runnerKey`, `bay.runnerName` and `bay.runnerSandbox`.
- `bay install` moves the old config keys to the new names; `bay doctor` names any left.
- A registry signed with an `executor` file keeps working; the next `bay enrol runner` rewrites it.
- `bay status --fleet` is `bay status --all`; `bay status --heads` is `bay status --branches`.
- `bay host init <path>` is `bay host <path>`.
- `bay hook` is no longer listed in `bay --help`; `bay install` writes it for the agent.
- `WORKSPACE_INCOMPLETE` is `VIEW_INCOMPLETE`.
- `retry_when` takes one of seven values: `by_itself`, `after_wait`, `after_fix`, `after_person`, `after_setup`, `after_new_main`, `never`.
- `bay help --json` lists agent products in `detail.agents`, not `detail.adapters`.
- Every change record is written as `bay.change/1`; records stored as `/2` to `/4` are still read.
- Every result sentence says what happened first, then why, then one step.
- Lock and offline refusals print one clean line.
- A lock is granted only on the newest copy of its file; an older copy is refused `INPUT_CHANGED`.
- A change to a lockable file merges only if it held the lock; otherwise `LOCK_NOT_HELD`.
- `bay save` takes the locks of the lockable files it saves; a released lock keeps its history.
- With `[plugin.assets]` and `over_limit = "offload"`, a new file over `max_file_mib` never enters Git.
- The plugin stores its bytes; `.bay-assets` names them by SHA-256 and size, at save, merge and checkpoint.
- `bay plugin test assets -- <command>` checks a store plugin: put, put again, get, unknown.
- Saved large files come back by hash: in another clone at `bay install`, in views, and read-only in checks.
- `examples/assets/folder-store` keeps large files in any folder, such as a shared drive.
- A save refused after a large file was stored leaves the folder as it was.
- Two changes adding large files never conflict on `.bay-assets`: it merges line by line.
- Main's commit keeps the agent's own commit message from the view, with its `Open:` lines.
- `bay start --paths .` (or `/`) is a view of the whole tree; it takes new files anywhere.
- A large file merges only when its store holds it; otherwise `ASSET_MISSING`.
- A store may purge a file's bytes and keep its line; a clone names it and goes on.
- `[assets] keep` in the policy says how long a store keeps generated files.
- `bay plugin test assets` also checks `exists`, and optional range `get` and `purge`.
- A merge's lock guard counts only the lock's current holder, and only if it named the change.
- A command name in a sentence is said as typed (`bay merge`, never "bay combine").
- Grok, which runs a project's Claude Code hooks, has its shell and file writes judged as Claude Code's.
- A check runs without a file its store purged; one purge never blocks later merges.

## 0.3.8

- A bay host no longer refuses a push when it judges two pushes at once.

## 0.3.7

- `bay save` and `bay start` take what the work is, in a few words; a card id still works.
- Saving again with the same words goes on with the same change.
- Each entry of `bay help --json`'s word list now says what it `means` and which names to `avoid`.
- Plain sentences lead with the effect: "Nothing was added: a check did not pass."
- A refusal without a code says "Nothing was changed:" and why, not "Something went wrong".

## 0.3.6

- `[locks] paths` in `.bay/policy.toml` names files that never merge, such as images or binaries.
- Each such file is edited in one session at a time, across machines.
- The first edit takes its lock on the remote; another machine's edit is refused `PATH_HELD`, naming the holder.
- A lock ends when its session stops, or after the takeover window.
- Offline, an edit of a locked path is refused `REMOTE_UNREACHABLE`.
- A runner never moves to a new bay that lacks its command; it keeps running and says why.

## 0.3.5

Results print the new words everywhere; scripts that read the old field names need the new ones.

- `freshness.trunk` is `freshness.main`; `detail.trunk` and `trunk_*` fields say `main`.
- `landed`, `landing` and `landings` fields say `merged`, `merge` and `merges`; `published` is `is_merged`.
- A change's state `landed` and an operation's `published` print as `merged`.
- `waiting_for` says `runner` and `runner_capacity`; `executors` in the fleet view is `runners`.
- Events `trunk_red`, `trunk_not_linear` and `unattributed_landing` are `main_red`, `main_not_linear` and `outside_merge`.
- The notice `landed_on_your_paths` is `merged_on_your_paths`; the warning `no_live_executor` is `no_live_runner`.
- `bay status --heads` classes main as `main` and merged branches as `merged`; doctor's `trunk-hook` row is `main-hook`.
- A completion profile's dimension `published` is `merged`, in results and in `[profile.<name>] require`.
- Notes, event ids and the hook's notice text say merge, main and runner too.
- The key class keeps its name, `executor`, wherever it is printed, as `bay enrol executor` takes it.
- Records already stored in Git keep their names and are still read.

## 0.3.4

- The README is rewritten for a newcomer: one word per idea, one branch, no agent branches.
- `bay help`, install and refusal texts say merge where they still said land.

## 0.3.3

- `bay enrol` refuses a key that was removed, under any name, before it signs or pushes anything.
- `bay doctor` is red, `REGISTRY_UNREADABLE`, when the key registry breaks a rule, and names the repair.
- `bay enrol --remove` repairs a registry that enrols a removed key again.
- Fixed: on a remote that is not a bay host, 0.3.2 could push a registry no bay command could read.
- The config names the branch bay serves `main = "<branch>"`; `trunk` is an unknown key now.
- Git config `bay.main` replaces `bay.trunk`. A bay host without it guards the branch its `HEAD` names.
- The skill and README say main is the one branch you name, and agents never create branches.
- They show a `dev → staging → production` repository: bay merges into `dev` and promotes along the chain.

## 0.3.2

- `bay enrol --remove <class> <name> --reason rotated|revoked` removes a key, and records why.
- A rotated key's records made before its removal still count; a revoked key's records count nowhere.
- The host refuses a registry version that undoes a removal, and a removed key is never enrolled again.
- A plain sentence names a broken file by its path, such as `.bay/config.toml`, not as bay's records.

## 0.3.1

- Every help text, next step, doctor note and the skill say merge, main, runner, plugin and machine.
- `bay help` shows values you may leave out, as `--wait[=<SECONDS>]`; `help --json` marks them `value_optional`.
- `bay help` names each argument by its placeholder, such as `<PUBLIC_KEY_FILE>`.
- `bay share` and a talk's send push all their refs or none.
- An unchecked merge no longer counts as a check run in `detail.flow`.
- A config may no longer name the bay it needs: `bay = "<version>"` is an unknown key now.
- A config that names another main says to run `bay install --main`.
- A shell command the hook denies names `SHELL_DENIED` and its next step.
- Every recorded agent payload is held to the hook contract: exits, reasons and time.
- `POLICY_INVALID` and the skill say how to repair a broken policy: a merge signed by the registry key.
- The skill says each agent sets `BAY_ACTOR` to its enrolled name under full signing.

## 0.3.0

One name per idea. Old names are removed, not aliased, so update scripts and settings files together.

- `bay land` is `bay merge`; the decision `landed` is `merged`.
- `bay watch` is `bay status --since [<cursor>]`; `--landed` is `--merged`.
- `bay executor` is `bay runner`, and `[executor]` in the config is `[runner]`.
- `bay adapter test` is `bay plugin test`; plugins are configured as `[plugin.<kind>] command, timeout_s`.
- `bay approve --reject` is `bay reject <change>`; `--trunk` is `--main` on `bay install` and `bay host init`.
- Gone: `start --resume`, `land --abandon`, `--observe`, `--intent-revision`, `status --stranded` and `status --heads --prune`.
- `[[freeze]]` is `[[hold]]`; a hold with no `target` holds main.
- Hook moments are renamed, such as `tool.before` and `turn.end`; `bay install` rewrites the entries.
- `bay doctor` names hook entries an older bay wrote, under `old_names`; a tool call under an old name is refused.
- A tool call refused under a wrong hook name says to run `bay install`, not to update bay.
- Reason codes say main and merge: `MAIN_MOVED`, `MAIN_RED`, `MAIN_HELD`, `CHANGE_NOT_MERGED`, `OUTSIDE_MERGE`, `RUNNER_KEY_MISSING`.
- `QUEUED` replaces two queue codes; `TALK_REFUSED` replaces seven talk codes, with `detail.why`.
- `TOOL_BELOW_FLOOR` and `TOOL_ABSENT` name the tool in `detail.tool`: git, bay, claude or codex.
- A lane change's checks end `ok` with the verdicts; it still never merges.
- Results no longer carry `published` or `next`; `receipts` is `evidence`.
- A throughput lever turns evidence reuse on; `evidence_reuse` is needed only for reuse alone.

## 0.2.17

- `bay status --fleet` shows each machine's last `bay doctor` verdict, and `ready` counts the fleet's green and red.
- `bay doctor` publishes this machine's verdict on its presence record, so the fleet view needs no ssh.
- `unattributed` counts the landings that went around bay per writer, under `by_writer`.
- `detail.flow` adds queue wait and check time, p50 and p90, from the records bay keeps.
- A config may name the bay it needs, `bay = "<version>"`; an older bay then says to update.
- `[policy] reuse_unsigned = true` lets unsigned evidence be reused and batched; results still name their trust.
- `bay install` and `bay update` put the skill in every Claude config directory found; doctor checks each one.
- `git config --global --add bay.claudeConfigDirs <dir>` names a config directory bay cannot find itself.
- `bay help` and `bay help --json` say what each command writes, and what it never writes.
- The hook refuses a force push spelled as a short-flag cluster, such as `-fu` or `-vf`, not only `-f`.
- A tool call that reaches `bay hook` under an unknown or wrong name is refused, never let through.
- `bay install` in a clone says when its hook entries still have to land on main.
- Doctor's and `HOOKS_MISSING`'s recovery name landing the settings file; `bay save`'s repair says `bay start <change>`.

## 0.2.16

- `bay help --json` declares the Codex adapter too: its events, timeout, and that its hooks fail open.
- Doctor's `off` hooks row names both agent products, `claude` and `codex`.
- A refused enrolment says "The approver name is a short lower-case word", not "A approver".
- `bay install --machine` says what it writes, and that it never writes hook entries or a project's settings.
- With a tracked `.claude/settings.json`, doctor's hooks recovery names landing the change that commits bay's entries.
- The skill gives the order for rolling bay's hooks out to many machines.

## 0.2.15

- One word list: `bay help --json` exports the words a person never sees, and the word said instead.
- A plain sentence says "main" for the trunk, never "the shared copy"; help text says main too.
- At a terminal, bay shows the plain sentence first and no JSON; a pipe or `--json` gets the document.
- `bay --help` is the same grouped page as `bay help`.
- "view" joins the words a person never sees; it is said as "folder".
- `bay help` shows `bay update --version`; next steps say `bay start <change>`, the one verb.
- A test budgets bay's commands, flags, reason codes and plain words; growth must say what it simplifies.

## 0.2.14

- `bay help --json` declares each agent adapter: its events, which can block, how each fails, and what it cannot see.
- An executor outcome the author must act on wakes them once through `[wake] command`; `land --wait` wakes nothing.
- `bay.wake/1` gains `cause`: `talk` or `outcome`; talk requests keep every field they had.
- `[agent] product = "codex"`: bay reads Codex's hooks as a sensor; they fail open, so main stays the guard.
- `AGENT_ABSENT` and `AGENT_BELOW_FLOOR` name a missing or old agent product other than Claude Code.
- `bay status` names the flow: live changes per actor, landings, cycle time, the oldest change and the approval queue.
- `[limits] max_waiting_approvals` caps how many changes wait for a yes; a change needing one is refused `WIP_LIMIT`.

## 0.2.13

- `bay help` shows every command, subcommand and flag on one page, grouped for people, agents and admins.
- `bay enrol <class> <name> <key.pub>` adds one key to the key registry; no Git plumbing is needed.
- The skill has an agent-led section for setting up bay for a team.
- `bay start <change>` picks up an existing change; `--resume` still works.
- `bay abandon [<change>]` gives a change up; `bay land --abandon` still works.
- `bay prune` deletes landed branches; `bay status` never deletes, and `status --heads --prune` still works.
- `bay status --heads` also names stranded local branches and worktrees whose folder is gone.
- After `bay approve`, `say` names who adds the change next; it no longer says nothing else is needed.
- A plain sentence names a Git path as "bay's records", never as a path that does not exist.
- `bay doctor` no longer calls a repository with no agent hook green; its hooks row reads `off`.
- Doctor is red when a trunk bay lands on also holds landings that went around bay, and counts them.
- `bay status` names `guard.hook`, and `unattributed` counts the landings that came `through_bay`.

## 0.2.12

- A land that loses a race on its change chain reads the chain again; it no longer fails.
- A resubmit while an executor records an attempt is now queued, with one request, never two.

## 0.2.11

- A check marked `waivable = true` can be waived on one candidate by an enrolled approver.
- `bay approve <change> --waive <check> --why <text>` records a signed waiver; a reason is required.
- `bay waiting` lists each waivable failed check with its real verdict and the command that waives it.
- A waiver is never a pass: the landing names it in `coverage_gaps` with the real verdict.
- Targets such as `staging` never count a waiver toward promotion.
- A refused check now shares its evidence, so an approver on another machine reads the real verdict.

## 0.2.10

- Under `signing = "approvals"`, a second machine now lands; it no longer fails with "producer local collides".
- A clone with no `bay.producerName` takes its own, `local-<id>`, and keeps it in its git config.

## 0.2.9

- Every refusal names its next step in `next_step`; `bay help --json` lists each reason's next step.
- `bay status` names the next step of a blocked operation.

## 0.2.8

- Reading a change's chain from the remote never moves local work back; no command forces it.
- Several executors on one queue no longer fail with "the change chain kept moving".
- A local chain head the remote moved apart from is set aside under `refs/bay/kept/`, never lost.
- A process's own chain writes the remote never took are taken back when it finishes.
- After a candidate moves, `bay approve` says the answer still holds while the approved files are unchanged.
- `--wait=SECONDS` watches only while the land waits in the queue for an executor; other results return at once.

## 0.2.7

- An approval now holds while the files it covers are unchanged; unrelated landings no longer ask again.
- A landing names each approval it carried from an earlier candidate under `carried_approvals`.
- When gated files change, `bay status` and the refusal name them under `set_aside`.
- `[[check]] carry = false` asks for an approval on every candidate.
- `allowed` now uses four words: read, refresh, repair, wait.
- `bay start <intent>` after a landing opens the next change; `--new` is no longer needed there.
- `bay land --wait=SECONDS` and `bay undo --wait=SECONDS` watch that long, then return while the run continues.
- `bay land <change>` takes the change positionally; `--change` and `--observe` still work.

## 0.2.6

- Without a key registry, an approval now blocks promotion and status the same way it already blocks landing.
- bay stops with an error when Git cannot find a commit, instead of treating it as not an ancestor.
- Policy reads file names with spaces or non-ASCII characters correctly.
- A hung adapter is killed with every process it started, never leaving children behind.
- `say` repeats a message's first sentence whole or not at all, so it never flips meaning.
- After `bay approve`, with an executor running, bay says the executor's next pass lands it.
- 0.2.5 was never released; its changes below arrive with 0.2.6.

## 0.2.5

- `bay waiting` lists the changes waiting for your approval, with candidate, paths and a diff summary.
- `bay approve <change>` signs and pushes your approval of the exact pending candidate.
- `bay approve --reject --why <text>` records a signed no; the change is blocked, naming you.
- Once a person approves a change, the executor's next pass lands it; nobody runs `bay land` again.
- If main moved after the approval, the change is rebuilt and waits for approval of the new candidate.
- Every result now carries `say`: one plain sentence saying what happened and what to do next.
- A policy `[limits]` table caps file size, files touched and added lines per change.
- A change over a limit is refused `CHANGE_TOO_LARGE` before any check runs.
- Checks can read box directories named with `bay.checkReadPath`, read-only; `bay doctor` lists them.
- `bay executor install` writes a launchd agent or systemd user unit for the executor, and starts nothing.
- Agents hear once when changes are queued and no executor is running.
- The skill tells agents how to work with people who are not developers: save, waiting, approve, undo.
- A replaced evidence record, such as an unsigned copy followed by the signed one, now counts as a change.

## 0.2.4

- `bay save <intent>` lands the edits made in the main checkout as one change.
- After a save lands, the main checkout fast-forwards to the new main when nothing is in the way.
- A refused save leaves the main checkout's files exactly as they were, and names the change if one opened.
- `[policy] signing = "approvals"` signs only approvals: small teams land without agent, executor or producer keys.
- Under approvals-only signing, landings and check evidence run unsigned and results say `trust: "approvals"`.
- Under approvals-only signing, evidence reuse and batching are off, because no producer signs.
- An approval still counts only when an enrolled approver's key signed it.
- `bay doctor` shows the signing mode.
- A dependency that ends `no_effect` is met: its dependent lands, never `DEPENDENCY_ABANDONED`.
- A check reading history with `baseline = "base"` now matches its base record by commit, never by tree.

## 0.2.3

- `bay start --after <change>` lands a change only after the named changes landed on main.
- A dependent change waits queued with `DEPENDENCY_PENDING` and holds no queue place meanwhile.
- A dependency that ends without landing refuses its dependent with `DEPENDENCY_ABANDONED`.
- The dependent's main commit names each dependency in a `Bay-After:` trailer.
- `[executor] parallel_checks = N` runs up to N of a candidate's checks at once; default 1.
- A check may set `materialize = "changed"` to see only the files a change touched, plus its `paths`.
- Such a check's request says `"materialized": "changed"`, and its evidence is keyed by commit.
- `bay status --heads --prune` runs an optional `[heads] on_prune` adapter after each delete, from trunk's copy.
- A failing prune adapter never undoes the prune or stops it; each branch records the adapter's outcome.
- `bay start --from <branch>` takes the remote's tip when it has one, and fails if the remote cannot be listed.
- A `--from` branch touching undeclared paths is refused as `PATHS_UNDECLARED`, naming them.
- bay finds landings that an outside merge pushed off trunk's first-parent line, by ancestry.
- Red judgement, culprits and targets read that landing line; undo finds such a landing by ancestry.
- `bay status` warns `trunk_not_linear` for such merges in the red window; `bay watch` names each new one once.

## 0.2.2

- A check request now names its base commit, the change, its actor and the change's commit messages.
- A check declaring `inputs = ["history"]` reads repository history through a temporary, read-only Git directory.
- History-reading checks are keyed by commit, so their evidence never stands for another commit.
- `bay start --from <branch>` takes in a branch made outside bay; it lands as one checked commit.
- `bay status --heads` lists every remote branch as trunk, target, lane, landed, stale or unique.
- `bay status --heads --prune` archives each landed branch under `refs/bay/archive/heads/`, then deletes it under a lease.
- Only branches shown merged by ancestry or equal patches are pruned, never by Change-Id alone.
- A `[[freeze]]` naming the trunk holds it: only fixes and reverts land until its end.
- A `[hold] command` adapter can hold the trunk; an unreadable answer counts as held.
- Held changes wait queued with `TRUNK_HELD`; `bay status` shows the hold in `detail.hold`.
- A check with `baseline = "base"` fails a change only on failures its base does not share.
- Pre-existing failures land named under `coverage_gaps` as `baseline_failures`, never as a pass.
- After `bay land --abandon`, `bay start <intent> --new` claims the next generation, superseding the abandoned change.
- `bay start --resume` refuses an abandoned change once a newer generation of its intent exists.
- Re-opening a change with plain `bay start` reports `behind`, the trunk commits it missed, with a hint to resume.

## 0.2.1

- A land whose `[executor] command` cannot start or refuses now ends `failed`, and nothing lands it later.
- That failure names the executor command and its reason, never `git`.
- The agent hook now denies a `git push` to any promotion target, in every refspec form.
- A `[[target]]` with `advance = true` is fast-forwarded by the executor to its newest promotable commit.
- A live change's session hears once when another change lands on its declared paths, and should refresh.
- `bay start` refuses `WIP_LIMIT` when an actor already holds `[agent] max_live_changes` live changes.
- Below the box's `bay.minFreeMiB` floor, `bay start` refuses `DISK_LOW`; a check there ends `infra_failure` naming it.
- `bay doctor` names a `gc.log` that blocks automatic garbage collection.

## 0.2.0

Scale
- An executor pass costs the same however many closed changes and finished queue places the repository holds.
- After the retention window, the executor removes finished queue places below the head, always keeping the highest.
- Terminal records and `bay status` show an operation's submit, queue, head, check and publish times.

Red trunk
- `[red] window = N` in `.bay/config.toml` sets how many trunk commits red and unattributed judgements read.
- A post check unanswered in that window shows as `red_pending`, never as clear; it refuses no landing.
- A red post check names its `suspect` range: the last pass, the first failure, and the landings between.
- `[executor] bisect_post = true` bisects a red post check's suspect range, recording each run as post evidence.
- The bisect reruns the suspect once to rule out a flake, in at most log2(n) + 1 runs.
- A red pinned to one landing names it `culprit` with each run's verdict; it owns the red and `TRUNK_RED`.

Promotion
- `[[target]]` in `.bay/config.toml` names promotion targets such as `staging`, with required checks and a soak.
- `[[freeze]]` holds a target until its end; meanwhile only a `Fixes:` or `Reverts:` landing is promoted.
- `bay status` lists each target's tip, drift, lag, checks, freeze and newest promotable commit under `detail.targets`.
- `bay watch` names a target that moves as `target_moved`, or `target_unpromotable` when the commit fails its rule.
- A bay host refuses a push that moves a target to an unpromotable commit, with `PROMOTION_REFUSED`.

Lanes
- `[[lane]]` in `.bay/config.toml` names sandbox lane refs, such as `refs/heads/research/*`.
- `bay start --lane <name>` opens a lane change: its checks run against trunk, and it never lands.
- A lane change's land is refused `LANE_NEVER_LANDS`, carrying its check verdicts; the host hook refuses it too.
- A change whose view reaches a lane's commits off trunk is refused `LANE_LEAK`, naming lane and commits.
- The agent hook denies merge, cherry-pick, checkout and restore from a lane ref in other views.
- `bay status`, the fleet map and `bay watch` name a forge merge of lane work as `lane_leak`.
- A lane change's view is released after the lane's shorter `window_s`.

Approval
- A `[[check]]` with `kind = "approval"` and `when_paths` needs a person's signed yes for those paths.
- A new `approver` key class in the registry signs `approval` records, and nothing else.
- An approver runs `bay evidence record --kind approval` on the exact candidate commit a land names.
- A change on those paths is held with `APPROVAL_PENDING` until an enrolled approver's record arrives.
- `bay status` and the refusal name who can approve and the candidate commit to approve.
- A rejection blocks the change as `CHECK_FAILED`, naming the approver.
- An approval signed by any other key never counts, and the host refuses it.
- Without a key registry, no approval counts, and the refusal says so.
- A change elsewhere lands without an approval; quarantine sets an approval check aside like any check.

Actor proof
- `[agent] require_actor_proof = true` in `.bay/policy.toml` refuses land `ACTOR_UNPROVEN` when actors share an agent key.
- The remedy is a hardware agent key, or one OS account per actor. Off by default.
- `bay doctor`'s key-custody row shows `require_actor_proof`, and whether land is refused on this box.

Overlaps
- `bay start` lists live changes declaring overlapping paths in `detail.overlaps`, with intent, actor and paths.
- Session start names the same overlaps for the view's change; neither refuses.
- A queued change's early merge conflict reaches its session's agent once, with the paths.

Throughput
- `[executor] ahead = N` checks queued entries ahead of their turn, on the bases the entries before them predict.
- An entry whose prediction held lands on that evidence with no new check run; a missed prediction is ignored.
- `footprint = true` on a policy check carries its record to a later candidate whose read set is byte-identical.
- `[executor] footprint_lanes = true` checks a queued entry beside the head when their paths are disjoint.
- Landings stay in queue order; a carried record names the record it was carried from.
- `[executor] batch = { min, max, max_failure_rate }` checks the queue head and entries behind it as one tree.
- A passing batch lands in one push; each member keeps its own commit, `Change-Id` and `Bay-Batch` trailer.
- A failing batch bisects on the same tip, lands its passing prefix, and checks the culprit alone.
- No batch forms while the lane's recent failure rate is above `max_failure_rate`, default 0.3.
- A bay host admits a `Bay-Batch` commit only within a whole batch under a config that sets `batch`.

Flaky checks
- `retry_on_failure = N` in a `[[check]]` reruns a failed check on the same tree, up to N times.
- A failure then a pass is `flaky`: every run stays in evidence, never a plain pass.
- `allow_flaky = true` lets a flaky check land, named in `coverage_gaps` with its runs; otherwise `CHECK_FLAKY`.
- `[[quarantine]]` takes `owner` and `since`; `bay status` lists each with owner, age and overdue.
- `bay status` counts each check's flaky landings over the red window under `detail.flakes`.

Executor presence
- Each executor pass publishes its presence: version, machine, environments, last pass, operation held and queue depth.
- `bay status --fleet` lists `executors`, each with its age and whether it is `stale`.
- `bay status` warns `no_live_executor` when operations are queued and no executor presence is fresh.
- `bay doctor --role executor` shows the executor's own presence and how its last push went.

## 0.1.15

- `bay land --wait` waits for another process's run of its operation and returns its outcome, not an early `ok`.
- `bay help --json` lists every command, flag and reason code, read from the binary itself.
- It includes the JSON Schema of `bay.result/1`, also kept as `schemas/bay.result.schema.json`.
- An executor pass costs the same however many closed changes and finished queue places the repository holds.
- After the retention window, the executor removes finished queue places below the head, always keeping the highest.

## 0.1.14

- `bay watch` never judges red from a registry it cannot read; it says `red_unread` and keeps its cursor.
- `bay watch` says `talks_unread` when it cannot read talks, instead of listing none.

## 0.1.13

- A live summary a blob-less cache cannot read is named in the fleet map, never shown as missing.
- `bay start` fails naming an unreadable live summary, never missing the session that holds the change.
- An unreadable `.bay/policy.toml` shows as `quarantines.unread` in `bay status`, never as no quarantines.
- A view's files hold their blobs' exact bytes, whatever `core.autocrlf` the box sets.

## 0.1.12

Partial clones
- An unreadable `.bay/policy.toml` is an error naming it: land never says POLICY_MISSING, status never reads red as clear.
- The same holds for the committed config, registry key files, executor logs, queue entries, evidence notes and requests.
- A red judgement bay cannot make shows as `red: {"unread": why}` in `bay status` and the fleet map.

Docs
- The README, skill and demo say main and merge; bay's commands keep trunk and land.

## 0.1.11

Talk
- The fleet map lists open talks, and each session counts the talks its actor owes and is owed.
- `bay watch` lists when one of your talks arrives, is answered, turns lost or stale, and closes.
- The fleet map's actors include the senders and recipients of open talks.
- A talk's row in `bay status` names the change a delivery talk is about.

Landing
- `bay status` shows a queued change its merge conflict with trunk before it reaches the head.
- The conflict's paths are in `operation.conflict`; the change keeps its place. It is not pushed to the agent.
- A resubmission racing a background executor on one machine lands once, never panics or leaves an operation running.
- An executor that faults after claiming an operation gives the claim back, so another takes it at once.

Views
- With `[view] warm_history = true`, `bay start` and `--resume` fetch the declared paths' history, and say how much.
- Below Git 2.54 the history warm-up is skipped, and the result names that floor.

Repository
- `bay host init` takes the object format of the repository it runs in, else Git's default.
- `log.showSignature` on a box no longer adds signature lines to what bay reads.

Adapters
- `bay adapter test check|wake|notify -- <command>` runs bay's contract cases against a connector, as bay runs it.
- Each case names what was sent, what came back and why; the command exits 0 only when all pass.
- A late, noisy or environment-dependent answer is named, with what bay would record for it.

Update
- `bay update` installs at once, even while an executor holds an operation; `UPDATE_IN_USE` is gone.
- The daily background check no longer waits for a running executor either.
- Each release is kept under `$XDG_DATA_HOME/bay/versions/`; updates keep the newest three and every running executor's.
- An executor's operation starts child `bay` processes from its own version, ending on the version it began with.
- On Unix, between operations, a looping executor execs its new binary and names the move in `detail.reexec`.

## 0.1.10

Speed
- `bay host init` sets the host to keep a commit-graph with changed-path Bloom filters, refreshed by Git's maintenance.
- `bay install` in a clone keeps its commit-graph fresh on fetch and writes the first one with Bloom filters.
- Evidence reads run two Git processes per tree, however many producers record evidence.

Doctor
- The `store` row reports the commit-graph: disabled, missing, no Bloom filters, or ok, and whether fetches keep it. Advisory.

Status
- `bay watch --since` judges a red trunk from its cursor, reading only new landings and moved evidence.

Partial clones
- On a blob-less cache, a view fetches only its declared paths and hook settings, also on resume.
- With Git 2.45+, Git never fetches a missing object inside bay's reads; bay fetches its files by name.
- A bay file a blob-less cache cannot fetch is an error naming the file, never a silent default.

## 0.1.9

Before you upgrade a bay-hosted trunk's clients, upgrade the host. Older hosts refuse an expired talk's close.

Status
- `bay status` and the fleet map name and count recent trunk merges and pushes that did not land through bay.
- `bay watch`, outside a view, lists an `unattributed_landing` event for each landing outside bay.

Landing
- Racing starts for one intent make one change: a same-name start adopts it, the others get `CHANGE_SUPERSEDED`.
- `bay undo` never rewinds this store's copy of the undone change's chain.

Talk
- A talk read after its `expires` closes as expired: the notice prints `EXPIRED`, and no reply is owed.
- Expiry is judged by the ack's commit time, so a talk read in time still owes its reply.
- `bay status` shows an unread talk past its `expires` as `expired`, never `lost`.
- `bay land` and `bay undo` list open talks about that change in `detail.open_talks`, and never block on them.

## 0.1.8

Speed
- Commands run two to four times faster: bay no longer waits a fixed 20 ms for each Git call.

Doctor
- `key-custody` lists the actors whose sessions run under this account, and how a signature proves who signed.
- Several actors under one account that can read the agent key make `key-custody` degraded, not red.
- A red `sandbox` row names the AppArmor profile fix when the kernel restricts user namespaces.

Views
- `bay start` releases clean views of landed, abandoned or no-effect changes once past the retention window.
- A view with changes, own commits, a lock, a live session, a broken worktree, or your directory is kept.

Evidence
- `bay evidence record` keeps a record whose push failed, and pushes it on the next run.
- It catches up when another clone recorded under the same name first, and reports a divergence.

Talk
- A reviewer's later finding on a change supersedes its open talk there, so the author sees one.

Sandbox
- On macOS, a check's own scratch under `/private/var` is no longer read as a reach outside.

Trunk health
- `bay status --fleet` lists the red checks, and marks each session or change working on red paths.
- `bay watch` lists `trunk_red` when a post check turns trunk red, and `trunk_red_cleared` when it clears.

## 0.1.7

Before you upgrade a bay-hosted trunk's clients, upgrade the host. Older hosts refuse an ack that names its wake.

Trunk health
- Added a red trunk: a failed post-publication check makes its paths red, owned by the landing that broke it.
- A landing on red paths is refused `TRUNK_RED`, naming the owner and the ways out.
- A fix, undo or supersede of the owner always lands. A later pass or a quarantine clears it too.
- `bay status` lists the red checks. `bay evidence record` takes `--environment` to answer a post check.

Talk
- `bay status` lists each open talk you sent or must answer: who owes the reply, ages, and standing.
- A talk with no ack within 24 hours is `lost`, and one past its `follow_up` is `stale`. Both stay listed.
- Added the wake adapter, `[wake] command`. Its answer is recorded on the ack, and the sender sees it.
- A wake adapter that fails or takes over 5 seconds is recorded `uncertain`. It is never called twice.
- Added `bay hook talk-wait`: an idle wait that polls for talks and exits 2 when one arrives.
- A live session's hooks fetch talks at most once every 30 seconds.

Doctor
- Changed `hook-overrides`: another tool's hooks for bay's events are listed, not red. They run beside bay's.
- `hook-overrides` is red only for `disableAllHooks` or `allowManagedHooksOnly`.

## 0.1.6

Before you upgrade a bay-hosted trunk: upgrade its host first. Older bays cannot read the new `no_effect` state.

Landing
- Added `no_effect`: a change whose work is already on trunk publishes nothing. Before, it published an empty commit.
- Changed a lost push reply with trunk then unreadable to `PUBLISH_OUTCOME_UNKNOWN`, never `failed`. The next land reconciles.
- Changed a held ref lock on the remote into a retry. A lock never released ends `PUBLISH_RETRY_EXHAUSTED`.
- Fixed an executor that retried forever while a reftable lock blocked every ref update.
- Fixed a merge Git could not read being reported as `MERGE_CONFLICT` with no paths. It is now a fault.
- Added `freshness` to the results of `start`, `land` and `undo`.
- Added `elapsed_s`, `receipts` and `next` to an asynchronous land, and `elapsed_s` and `next` to status.

Evidence and review
- Added `bay evidence record`, the write path for an outside producer or reviewer.
- Records are signed whole, findings included. A rerun writes nothing, and records over 64 KiB are refused.
- Added `bay.reviewerName` and `bay.reviewerKey`. A review record is signed by the reviewer key only.
- Added reviewer delivery talks: a reviewer tells a landing's author what it found, with no agent key.
- A reviewer talks only about a landing it holds a review record on.
- Added `[[dimension]]` to the policy: a site's own completion dimension, such as `reviewed`.
- A profile may require a site dimension. The landing verdict never reads it.
- A rejection is answered only by a landing that names the change, never by a later pass.
- Changed evidence readers to ignore a record of a schema major they do not know. Executors refuse such a request.

Updates and robustness
- Changed `bay update` to step through the newest release it can verify after a key rollover.
- Every file bay replaces now survives a crash whole.
- Every Git call runs in the C locale, so translated Git messages are read correctly.
- Fixed a check whose shell could not start being reported as reaching outside its sandbox.
- Added doctor rows: the object store and its maintenance, and a hosted trunk on the `files` ref format.
- The doctor `key-custody` row also covers `bay.reviewerKey`.
- `scripts/release.sh publish` tags the commit it built, even if trunk moved since.

Repository
- Added CONTRIBUTING, SECURITY, a code of conduct, issue and pull request templates, and a public AGENTS.md.

## 0.1.5

The first release signed by the hardware release key. Every release before it was signed by the old key.

Checks
- Fixed checks on macOS 27, where every check aborted at start under the 0.1.4 sandbox.
- Added a unit `reason` to check results. A refusal and `bay status` show why each failed unit failed.
- A reason over 2 KB makes the run `infra_failure`. bay never cuts a reason or copies raw check output.

## 0.1.4

Builds for Linux (x86_64 and aarch64, musl) and macOS. Apple silicon is tested; Intel (x86_64) is built but not yet run on an Intel Mac.

Known issue: on macOS, checks fail to start (`CHECK_UNKNOWN`). Use 0.1.3 on Macs, or 0.1.5.

Before you upgrade an executor: checks now run in an allow-list sandbox with no network. A check that reads outside its candidate and adapter now fails as `infra_failure`. List extra files in `[[check]] paths`. Set `network = true` in the base policy for a check that needs the network.

Release keys
- Added a second trusted release key, held in hardware, beside the current one. This release is signed by the current key.
- Changed `bay update` to trust a list of release keys, so a key can roll over. A later release drops the old key.
- Added a doctor `key-custody` row and `KEY_EXPOSED`. It flags a signing key that agent sessions on this account can read.
- Added a `reviewer` key class. A `review` record verifies only against the reviewer file, and a reviewer key never signs a check.
- Changed the registry read to refuse a reviewer name that is also a producer or an agent.

Checks
- Changed the check sandbox to an allow-list. A check sees its candidate, its adapter and system files, never `HOME` or `.git`.
- Changed checks and input providers to run with no network unless `[[check]] network = true` says so at the base.
- Added `stage = "post"` to `[[check]]`. A post-publication check never blocks a landing, and status shows it `pending` until its record arrives.
- Added `when_paths` to `[[check]]`. The check runs only when the candidate's own diff touches a matching path.
- Changed a policy or adapter change to require every check, whatever its `when_paths`. A skipped check is listed under `not_required`.
- Added `producers` to `[[check]]`. A record from a producer the list leaves out does not count.
- Added `bay.checkEnvironment`, the environments a box serves. An operation no box can serve waits with `CHECK_UNAVAILABLE`.
- Added the adapter's `unavailable` answer. It writes no evidence and never counts toward a queue deadline.
- Changed materialization in a blob-less clone to fetch all absent blobs in one batch, not one fetch per file.
- Added `refs/bay/inflight/<op id>`, a local ref that keeps a candidate safe from maintenance until its operation ends. It is never pushed.

Landing and history
- Added `bay start <intent> --fixes <change>`. The landing carries a `Fixes:` trailer, and an unlanded target is `CHANGE_NOT_LANDED`.
- Added `CHANGE_HELD`. A same-name `bay start` waits while another live session holds the change, instead of adopting it.
- Added `EVIDENCE_WITHDRAWN`. A bay-hosted trunk refuses a notes push that drops or changes an earlier record.
- Added `trust` (`unsigned` or `signed`) to the land result, check refusals and `bay status`.
- Added `require_signed = true` to a completion profile. Then a dimension that rests only on unsigned records is `unknown`.

Hooks and sessions
- Added `HOOK_INPUT_INVALID`. The pre-tool hook denies a payload it cannot read.
- Changed the shell deny list to catch denied Git commands inside `bash -c`, `sh -c`, `env`, `xargs`, `sudo` and `exec`.
- Added `SHELL_DENIED` and Agent Hooks verdicts to session rows. A denied command is recorded by its shape, never its text.
- Added `PATH_HELD`. Two sessions in one tree cannot overwrite each other's claimed paths; other views are never refused.
- Changed `bay install` to write a 30-second timeout on the pre-tool hook, so a hung hook denies quickly.
- Changed the installed hook command to exit 0 on a box without bay. Where bay is installed, its deny still blocks.
- Changed views to hold `.claude/settings.json`, so a session started in a view runs bay's hooks.
- Added draft checkpoints at `SessionEnd` and after an idle interval (`[hooks] checkpoint_idle_s`). A killed session loses at most one interval.
- Added a `SessionStart` notice when the tree is behind trunk on its instruction files (`AGENTS.md`, `CLAUDE.md` by default).
- Added a notice, given once, when a talk to the session's actor arrives.
- Added `permission_mode` to session rows and the fleet map. bay's deny still holds under `bypassPermissions`.
- Added `actor_source` to results and session rows, and `[agent] require_actor = true`, which refuses `ACTOR_UNNAMED`.

Status, watch, talk and evidence
- Added `bay status --stranded`: local branches and worktrees whose commits no remote holds. It reads only.
- Added `bay status --lineage [<range>]`: the `Reverts:`, `Supersedes:` and `Fixes:` pairs on a trunk range.
- Added `behind_commits` and `session_age_s` to each session in `bay status --fleet`.
- Added `bay watch --landed`: landings after a cursor, each with its base, actor and time. Commits outside bay are marked.
- Added `bay evidence missing --kind <k> --producer <p>`: trunk commits no record of that kind and producer covers.
- Added `bay talk --to-change <change>`. It reaches the change's lander from bay's record, and refuses an `UNATTRIBUTED_LANDING`.
- Added the `answered` talk close. A delivery talk closes when a fix, revert or supersede of its change lands.

Doctor
- Added `bay doctor --role executor`. It judges the box as an executor and exits 10 when a row is red.
- Added doctor rows `agent-hooks`, `hook-overrides`, `repository`, `environments` and `config`.
- Changed `bay executor run` to refuse with `GIT_BELOW_FLOOR` below Git 2.50.

Configuration
- Changed `.bay/policy.toml` to refuse an unknown key with `POLICY_INVALID`, naming the key, its table and its line.
- Retired `[check] command` in `.bay/config.toml`. The policy names each check's command, and doctor lists the old key as ignored.

## 0.1.3

Builds for Linux (x86_64 and aarch64, musl) and macOS. Apple silicon is tested; Intel (x86_64) is built but not yet run on an Intel Mac.

- Changed where bay reads `.bay/config.toml`: only from the trunk's committed copy, never from a work tree or a change.
- Added `bay install --trunk <name> --remote <name>`, which records where bay finds the trunk on this clone.
- Added `CONFIG_MISSING`: a repository with no config at its trunk is refused, naming where bay looked.
- Added `POLICY_MISSING`: a change never lands without a policy at its base, unless that policy says `unchecked = true`.
- Changed checks to materialize the candidate without a tar of the whole tree, so large repositories check quickly.
- Added `paths` and `materialize_timeout_s` to `[[check]]`; a materialization that runs over is `infra_failure`.
- Added the bay skill under `CLAUDE_CONFIG_DIR` when it is set, and doctor checks it there.
- Changed `bay talk --help` to say agent, not actor.
- Changed `bay update` to take the newest release that has a build for this machine. Releases are no longer split into stable and latest.
- Fixed the bay skill: it now lists watch, talk, status --fleet and land --wait. 0.1.2 had them, but its skill told agents not to run them.
- Removed update channels: `--channel`, `BAY_UPDATE_CHANNEL` and `bay.updateChannel` are no longer read, and doctor names one that is still set.

## 0.1.2

Builds for Linux (x86_64 and aarch64, musl) and macOS. The macOS builds were added on 19 October 2026, from the same commit. Apple silicon is tested; Intel (x86_64) is built but not yet run on an Intel Mac.

- Added `bay status --fleet`: every session on every machine, from signed live summaries, with its change, state and age.
- Added `bay watch [--since <cursor>]`: landings, events and invalidations since a cursor, so a missed notice never hides one.
- Added `bay talk`: a structured ask to another actor, acked by the recipient's own key, with supersede and answers.
- Added named inputs: `bay start --input <kind>:<name>`, and land refuses work whose inputs moved until it is repaired.
- Added the stale-write refusal: the Claude Code hook refuses a write that a stale input covers, from local data only.
- Added the Stop refusal: a session is asked once to checkpoint before it stops with unsaved edits in its view.
- Added hook notices: a refusal or failed check reaches the session once, as one line, within a byte budget.
- Added evidence reuse with `evidence_reuse = "exact-inputs"`: a check on identical inputs is not run again.
- Added runtime evidence and completion profiles, so status never calls a change done while a required part is missing.
- Added `BAY_UPDATE_VERSION` and `BAY_UPDATE_CHANNEL`, which pin one process's updates on a shared login.
- Added a session work record per agent that lists the files each step wrote.
- Changed checks to fail as `infra_failure` on a box with no sandbox, unless the box sets `bay.executorSandbox none`.
- Changed bay to compile for Windows; Windows builds come in a later release.
- Fixed the release script: a new MINOR or MAJOR needs the owner's approval through `BAY_RELEASE_APPROVED`.

## 0.1.1

Builds for Linux (x86_64 and aarch64, musl) and macOS. The macOS builds were added on 19 October 2026, from the same commit. Apple silicon is tested; Intel (x86_64) is built but not yet run on an Intel Mac.

- Added `bay undo <change>`: it lands a new, checked change that reverses one landing; history stays.
- Added check cost: each evidence record keeps the check's seconds, and `bay status` sums them per operation and landing.
- Added `bay update`: it replaces bay with a signed, verified release and never with an unverified download.
- Added a daily background update check from `bay status` and `bay doctor`, off with `BAY_DISABLE_AUTOUPDATE=1`.
- Added box-level update settings in global Git config: `bay.updateVersion`, `bay.updateChannel` and `bay.updateAuto`.
- Added `UPDATE_IN_USE`: an update waits while an executor on the box holds an operation.
- Added `bay install --machine`, which installs only the binary and the bay skill.
- Added the bay skill to `bay install`, and a doctor row that reports a skill that does not match.
- Added `bay status --fleet`, which fails closed until the fleet refs have been fetched.
- Added a `Bay-Session` trailer on landed commits; the actor trailer now carries the bare actor name.
- Added `changed_paths` to the check request, so a check can judge only what a change touched.
- Added `POLICY_INVALID` for a policy bay cannot judge by, and `declared_empty` for a check that selects nothing.
- Added `[[quarantine]]` to set aside a check that is already red on trunk.
- Changed checks to start from a fixed environment, which is part of each check's definition.
- Added `CHECK_UNAVAILABLE`: a queue head with no capable worker waits outside the queue.
- Added `REGISTRY_CHANGED` handling: a revoked key re-signs the same tree and reruns only commit-keyed checks.
- Changed the bay skill to list only the commands this version has.
- Changed `bay doctor` to say "Claude adapter not configured" when the adapter is absent.
- Changed `bay doctor` outside a repository to report the box's own rows instead of an error.
- Changed `.bay/config.toml` loading: an unknown key is an error that names it.
- Changed session start to report the change's state and open refusals from local data only.
- Fixed `bay host init` on Git below 2.54: it refuses with `GIT_BELOW_FLOOR` and creates nothing.
- Fixed a cancel or newer revision that races a landing: the losing operation says it was superseded.
- Fixed a request signed under a registry version that is not on the registry chain: it is refused.
- Changed evidence reading: fields a later version adds are kept and signed over, so newer records still verify.
- Fixed release archives: they carry no extended attributes, so Linux tar unpacks them without warnings.

## 0.1.0-pilot.1

- Added the first build: `bay start`, `land`, `status`, `doctor`, `install`, `share`, `host init` and `executor run`.
- Added signed release assets: one archive per platform, `VERSION`, `SHA256SUMS` and `SHA256SUMS.sig`.
- Added macOS and Linux builds for ARM and Intel.

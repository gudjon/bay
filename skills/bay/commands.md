# bay: more commands

Read with `SKILL.md`. The daily loop's other rows, then notes on research files, main's changes, questions, overlaps and `bay help`.

| When | Run |
| --- | --- |
| Take another actor's change and its view over, when that actor gave it up or is gone | `bay start <change> --adopt` (you become its actor; `detail.adopted_from` and `detail.view_adopted_from` name the one before). A change its actor still holds is taken only when the takeover window passes with no work from that actor; work in it gives `CHANGE_SUPERSEDED` |
| Try work that must never merge, in a sandbox lane the config declares | `bay start <intent> --paths <paths> --lane <lane>` (`bay merge` runs its checks; it never merges) |
| Take in a branch made outside bay (a bot's or a person's) and merge it in one step | `bay merge --from <branch>` (its intent is the branch's name). To take it in first: `bay start <intent> --from <branch>` (the remote's tip when the remote has the branch; it declares the files the branch changes, or give `--paths` covering them; then `bay merge`). After a rebase: `bay start <change> --from <branch or sha>` replaces its draft, no new branch name. Run again after its change ended unmerged, it opens a new change from the branch's tip. Main's commit keeps the branch's author and messages |
| Save edits made in the main checkout (non-developers) | `bay save <intent>` |
| Save only the edits under some paths, from any checkout (your own worktree too) | `bay save <intent> --paths <path>...` (the change declares those paths; your checkout's HEAD and files stay as they are; a path with no edit is refused) |
| See the changes waiting for this machine's approver: candidate, paths, diff summary | `bay waiting` (`detail.waiting`) |
| Approve one, or say no, when the approver asks you to | `bay approve <change> [--why <text>]`, `bay reject <change> [--why <text>]` |
| Waive one failed check marked `waivable`, when the approver asks you to | `bay approve <change> --waive <check> --why <text>` (that candidate only; never a pass) |
| Add one key to the key registry, when the user sets up keys | `bay enrol <class> <name> <key.pub>` (class `registry`, `approver`, `reviewer`, `producer`, `runner` or `agent`; signed by `bay.registryKey`) |
| Remove a key, when the user asks: `rotated` keeps its past records valid, `revoked` (compromised) voids them all | `bay enrol --remove <class> <name> --reason rotated\|revoked` (signed by `bay.registryKey`; the last registry key is never removed) |
| See the flow: live changes by actor and their limit, merges this week, and their cycle time, queue wait and check time (p50, p90); the oldest live change; what waits for a yes | `bay status` in the main checkout (`detail.flow`; a read of one change, or in a view, leaves the fleet's sections `null`); one change's own times and each check's runs: `bay status <change>` (`operation.timing`); a change in line names its place and how many are ahead (`operation.line`), and one a runner holds, how long since its claim (`operation.claimed_s`); when a live change is on main, from its stage (`draft`, `sent`, `in_line`, `at_head`), as low, likely and high, with each stage left, the merges it stands on and what would bring it sooner (`eta`: `on_main_at`, `stages`, `basis`, `speedups`; null with `why` when too few merges measure it); when it was started (`started_at`) and what the session holding it did last (`last_activity`: `edit`, `check`, `commit`, `bay`, `command`, `read`, `idle`, `waiting`); where the last six hours' time went: merges an hour, the wait for a first claim, work done again (`rework_s`, by cause), the last step to main, and how often the estimate held (`detail.flow.recent`, `estimates`) |
| See each promotion target (`staging`): its tip, `drift`, `lag`, checks, freeze and the `promotable` commit | `bay status` (`detail.targets`) |
| See whether main is held, by whom (`source`: `window` or `plugin`), why and until when | `bay status` (`detail.hold`) |

Research, a plan or a review a person should see is a
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

# bay: reading every result

Read with `SKILL.md`, whose "Read the result" names the fields every result carries.

- `bay status` lists in `detail.red` each check that turned main red after a merge, with its `culprit`
  once the evidence names one merge. If that is yours, fix it first: `bay start <intent> --fixes <change>`.
  With no culprit, the row's `merge` and `suspect.first_failed` are only where the check was seen failing,
  not a proven cause, and `say` says so: the cause is one of `suspect.merges`, on the check's paths or not.
  With no pass in its `[red] window`, bay reads back down to `[red] reach` merges (default 250) for the
  newest pass; with none, `suspect.last_passed` is `null` and `suspect.searched` says how many it read.
  With `[red] undo_after_s` in `.bay/config.toml`, a runner undoes that culprit that many seconds after its owner was told,
  unless a change in line fixes or undoes it; the owner is told the undo, or why bay could not make it.
- While a change waits in the queue, `bay status <change>` may show `operation.conflict`: main
  moved into a `MERGE_CONFLICT` on the listed paths. It keeps its place; refresh and repair now.
  The agent hook also tells your session once.
- A `merged_on_your_paths` notice: another change merged on paths you declared. Refresh, re-read
  the named paths, then go on.
- `UNDOES_MERGED`: your change holds a file as it was before another change merged. `bay merge` says so before it queues, and a runner checks again at its turn. `bay status <change>` names the
  files and that change in `operation.undoes`, each other change in `undoes.also`: bring in main, keep that work, then `bay merge`.
- `DISK_LOW`: the disk is below this machine's floor, 5 GiB unless `git config bay.minFreeMiB` sets another; `0` turns
  it off. `bay start` already freed what it could, and `detail.released_views` names those views. Free more, then start again.
- `BOX_BUSY`: the runner's machine is at or above the load limits it sets (`git config bay.checkStartLoad1`,
  `bay.checkStartCpuSome`), so it starts no check there; `detail` names the load and the limits. Nothing to do.
- `EXPEDITE_REFUSED`: `bay merge --expedite <why>` could not put the change next. It waits its turn as usual;
  `detail.expedite.refused` says why: `off` (no `[expedite]` in the config on main), `live` (another expedite waits),
  `spent` (the UTC day's `per_day` is used; `resets_at`). Do not ask again for it.
- Main is red on your paths (your change's row in `bay status` names the check in `red_on_paths`): nothing waits. Your change runs that check
  before it merges, judged against main, and merges unless it adds a failure; its result names it in
  `detail.red_checked`. A change that fixes it goes ahead of the line: `bay start <intent> --fixes <check>`, the check
  `detail.red[].check` names (the one name that never moves). Main is then `pending_fix` in `detail.red_pending` until
  a run of the check on the fix answers it.
- A change that no check covers, such as a document no check reads, is on main when `bay merge` returns, unless this
  machine has no runner key where the repository signs everything. `bay start` says
  beforehand, in `detail.pre_merge`: `at_once`, and in `checks` each pre-merge check a file under your declared
  paths can wake, with that path and an example file. To stay fast, narrow your paths. `bay doctor` names a
  check every change waits for (`pre_merge`).
- A session that starts outside a view hears nothing unless something waits on it, such as an open refusal of
  its own change. Without `BAY_ACTOR`, it also hears that it acts as the user name. Run `bay status` before
  you read refs or logs by hand.
- A session keeps the name it started with when it is resumed, even with no `BAY_ACTOR`. A resume that sets
  `BAY_ACTOR` to another name changes it.
- A plain `bay status` says in `say` who owes the next step of your open changes, and names those that need a
  fix or a person's yes, and the talks you owe a reply (`detail.talks_owed`). "Nothing needs you" means bay is
  moving the rest. Read talks this way, never from their refs.
- `bay status` describes your own changes and counts the others; `bay status <change>` reads one.
- `bay status` with `detail.warnings` naming `no_live_runner`: work is queued and no runner has
  been seen recently. Tell the user a runner must run (`bay runner run`, or
  `bay runner install` to keep one running). The work stays queued. The agent hook says the
  same once, at session start or a prompt: "bay: no runner is running for this repository".
- `bay status` waits for the remote at most `--deadline` seconds (default 3), and none when the
  last sweep is under `[status] serve_stale_s` old (default 60). Then the read answers from the
  store as that sweep left it: `detail.stale_s` says how old, and a `stale_read` warning names the
  sweep that runs on. Act on it: your own pushes are in it, and `bay merge` reads the remote
  itself. Read again later for a fresher answer; do not raise the deadline to wait it out. When the warning names `unreached`, this machine has
  not reached the remote since `unreached.since`: tell the user its network is down. Your work
  stays safe locally; nothing about the queue is known here until the remote answers.
- `bay status` with `detail.saved_s`: nothing in this clone moved since that answer was built,
  that many seconds ago, so it came back at once. Act on it as on any other read.
- `bay status` with `detail.warnings` naming `main_not_linear`: a merge pushed outside bay put
  merges off main's first-parent line. bay still finds them. Tell the user to require linear
  history on the host.

A few reasons need more than their next step:

| Reason | Also |
| --- | --- |
| `CHECK_FAILED` | The sentence names the failed unit and its first line; `detail.checks[].runs_as_main` lists the check's own files this change edits: the check ran as main has it, so merge that edit on its own first. A failed check ends the run: each check after it is `not_run` in `detail.checks`, never report it passed. A check with `baseline = "base"` names only new failures; pre-existing ones appear as `baseline_failures` in `coverage_gaps`: report them, never as passed. A check with `waivable = true` may instead get an approver's waiver (`bay waiting` lists it); then `bay merge --wait`. The waiver appears in `coverage_gaps` with the real verdict: report it, never as passed. |
| `CHECK_FLAKY` | A merge may name one flaky in `coverage_gaps`: report it, never as passed. |
| `WIP_LIMIT` | From `bay start`: you hold the most live changes (`detail.live_changes`). From `bay merge`: the review queue is full (`detail.review`: `waiting`, `max`, `oldest`); do `detail.next`. |
| `POLICY_INVALID` | While main's newest policy breaks a rule, bay judges each change by the last valid policy on main and names it in `detail.policy_used`: report that. A fix to the policy then merges like any change; bay raises `revision` above main's itself. Your own change's broken policy, or a `.bay/config.toml` that does not load, is refused: repair it. While main's config does not load, bay uses the last valid one and names it in `detail.config_used`. Only when main never had a valid policy is every merge blocked, its repair too. That repair merges when signed by the registry key: tell the user; on the machine that holds it, `git config bay.agentKey <registry key>` for that one `bay merge`, then set it back. Never do this unasked. On a main from `bay host` this path is not yet proven: tell the user so. Under `signing = "approvals"`, a fix to only the policy and its check scripts runs its own scripts and waits for an approver's yes instead; when main's policy asks no approval, an approver may waive each of main's checks on that fix (`bay waiting`), never as a pass. |
| `VIEW_NOT_YOURS` | `detail.view_actor` made the view, `detail.actor` is you (`detail.actor_source`: from `BAY_ACTOR`, or the user name). Take the change over only when that actor gave it up or is gone: ask it first (`bay talk`). Sessions with no `BAY_ACTOR` on one account are one actor. |
| `APPROVAL_PENDING` | Tell the user who can approve (`detail.approval.checks[].approvers`). An approval holds while its paths are unchanged (`checks[].set_aside` names the ones that changed). Never approve for them. |

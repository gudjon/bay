# Changelog

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
